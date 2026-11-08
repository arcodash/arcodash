// Typed API client for E2E setup/teardown.

const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8080";

/** The seeded target Postgres from docker-compose.testing.yml. */
export const TARGET_DS_OPTIONS = {
  host: "localhost",
  port: 5434,
  dbname: "target_db",
  user: "target",
  password: "target",
};

type Headers = Record<string, string>;

export class E2EApiClient {
  private sessionCookie = "";
  private csrfToken = "";
  private credentials: { email: string; password: string } | null = null;

  private async fetch<T>(
    path: string,
    init?: RequestInit & { headers?: Headers },
    retried = false,
  ): Promise<T> {
    const method = (init?.method ?? "GET").toUpperCase();
    const isMutating = !["GET", "HEAD"].includes(method);
    const res = await fetch(`${BACKEND_URL}${path}`, {
      ...init,
      headers: {
        Accept: "application/json",
        ...(init?.body ? { "Content-Type": "application/json" } : {}),
        ...(this.sessionCookie ? { Cookie: this.sessionCookie } : {}),
        ...(isMutating && this.csrfToken
          ? { "X-CSRF-Token": this.csrfToken }
          : {}),
        ...(init?.headers ?? {}),
      },
    });

    const setCookie = res.headers.get("set-cookie");
    if (setCookie) {
      const cookies = setCookie.split(/,\s*(?=[a-zA-Z])/);
      const cookiePairs = cookies.map((c) => c.split(";")[0].trim());
      this.sessionCookie = cookiePairs.join("; ");
      const csrfEntry = cookiePairs.find((c) => c.startsWith("csrf_token="));
      if (csrfEntry) {
        this.csrfToken = csrfEntry.split("=")[1];
      }
    }

    if (!res.ok) {
      let msg = res.statusText;
      try {
        const body = await res.json();
        if (typeof body?.message === "string") msg = body.message;
      } catch {}
      // A session revoked mid-run (e.g. by another worker's logout) is re-established once.
      if (res.status === 401 && !retried && this.credentials && msg.includes("not authenticated")) {
        await this.login(this.credentials.email, this.credentials.password);
        return this.fetch<T>(path, init, true);
      }
      throw new Error(`API ${method} ${path} → ${res.status}: ${msg}`);
    }

    if (res.status === 204) return undefined as T;
    return res.json() as Promise<T>;
  }

  private post<T>(path: string, body?: unknown): Promise<T> {
    return this.fetch<T>(path, {
      method: "POST",
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  }

  private del<T>(path: string): Promise<T> {
    return this.fetch<T>(path, { method: "DELETE" });
  }

  /** Returns the raw HTTP status for a GET — useful for authz assertions. */
  async getStatus(path: string): Promise<number> {
    const res = await fetch(`${BACKEND_URL}${path}`, {
      headers: {
        Accept: "application/json",
        ...(this.sessionCookie ? { Cookie: this.sessionCookie } : {}),
      },
    });
    return res.status;
  }

  async login(email: string, password: string) {
    const user = await this.post<{ id: number; email: string }>("/api/login", {
      email,
      password,
    });
    this.credentials = { email, password };
    return user;
  }

  async logout() {
    this.credentials = null;
    return this.post<void>("/api/logout");
  }

  async getSession() {
    return this.fetch<{ user: { id: number; email: string } }>("/api/session");
  }

  async createDataSource(body: {
    name: string;
    type: string;
    options: Record<string, unknown>;
  }) {
    return this.post<{ id: number; name: string }>("/api/data_sources", body);
  }

  async deleteDataSource(id: number) {
    return this.del<void>(`/api/data_sources/${id}`);
  }

  async listDataSources() {
    return this.fetch<{ data_sources: { id: number; name: string }[]; can_create: boolean }>(
      "/api/data_sources",
    );
  }

  async createQuery(body: {
    data_source_id: number;
    name: string;
    query: string;
    tags?: string[];
  }) {
    return this.post<{ id: number; name: string }>("/api/queries", body);
  }

  async updateQuery(
    id: number,
    body: Partial<{
      name: string;
      description: string;
      query: string;
      tags: string[];
      expected_version: number;
    }>,
  ) {
    return this.post<{ id: number; name: string }>(`/api/queries/${id}`, body);
  }

  async deleteQuery(id: number) {
    return this.del<void>(`/api/queries/${id}`);
  }

  // Publish a query's working version; required to schedule it, attach a monitor/widget, or reach
  // it without a grant.
  async publishQuery(id: number): Promise<void> {
    const full = await this.fetch<{ working_version: number }>(`/api/queries/${id}`);
    await this.post(`/api/queries/${id}/publish`, { expected_version: full.working_version });
  }

  async executeQuery(id: number, params?: Record<string, unknown>) {
    return this.post<{ job?: { id: string }; query_result?: unknown }>(
      `/api/queries/${id}/results`,
      { parameters: params ?? {} },
    );
  }

  async inviteUser(body: { name: string; email: string; groups?: number[] }) {
    return this.post<{ id: number; email: string }>("/api/users", body);
  }

  async acceptInvitation(token: string, name: string, password: string) {
    return this.post<{ id: number; email: string }>(
      "/api/users/invitation/accept",
      { token, name, password },
    );
  }

  async deleteUser(id: number) {
    return this.del<void>(`/api/users/${id}`);
  }

  async resendInvitation(id: number) {
    return this.post<{ id: number; email: string }>(`/api/users/${id}/resend_invitation`, {});
  }

  async revokeInvitation(id: number) {
    return this.del<void>(`/api/users/${id}/invitation`);
  }

  async listUsers() {
    return this.fetch<{ id: number; email: string }[]>("/api/users");
  }

  async listGroups() {
    return this.fetch<{ id: number; name: string }[]>("/api/groups");
  }

  async createGroup(body: { name: string; permissions?: string[] }) {
    return this.post<{ id: number; name: string }>("/api/groups", body);
  }

  async deleteGroup(id: number) {
    return this.del<void>(`/api/groups/${id}`);
  }

  async addGroupMember(groupId: number, userId: number) {
    return this.post<void>(`/api/groups/${groupId}/members`, { user_id: userId });
  }

  async addGroupRole(groupId: number, roleId: number) {
    return this.post<void>(`/api/groups/${groupId}/roles`, { role_id: roleId });
  }

  async removeGroupRole(groupId: number, roleId: number) {
    return this.del<void>(`/api/groups/${groupId}/roles/${roleId}`);
  }

  async assignGroupDataSource(groupId: number, dataSourceId: number) {
    return this.post<void>(`/api/groups/${groupId}/data_sources`, {
      data_source_id: dataSourceId,
    });
  }

  /** `restricted=true` scopes the group to its data-source allowlist; `false` (the default)
   * gives the group every data source in the org. */
  async setGroupDataSourceRestricted(groupId: number, restricted: boolean) {
    return this.fetch<{ ok: boolean; restricted: boolean }>(
      `/api/groups/${groupId}/data_source_access`,
      { method: "PUT", body: JSON.stringify({ restricted }) },
    );
  }

  async listGroupDataSources(groupId: number) {
    return this.fetch<{
      restricted: boolean;
      entries: { id: number; data_source_id: number; group_id: number; name: string }[];
    }>(`/api/groups/${groupId}/data_sources`);
  }

  async listRoles() {
    return this.fetch<{ id: number; name: string; type: string; permissions: string[] }[]>(
      "/api/roles",
    );
  }

  async createRole(body: { name: string; permissions: string[] }) {
    return this.post<{ id: number; name: string; permissions: string[] }>("/api/roles", body);
  }

  async deleteRole(id: number) {
    return this.del<void>(`/api/roles/${id}`);
  }

  async createVisualization(body: {
    query_id: number;
    type: string;
    name: string;
    description?: string;
    options: Record<string, unknown>;
  }) {
    return this.post<{ id: number; query_id: number; type: string; name: string; options: Record<string, unknown> }>(
      "/api/visualizations",
      body,
    );
  }

  async updateVisualization(id: number, body: Partial<{ name: string; options: Record<string, unknown> }>) {
    return this.post<{ id: number; name: string; options: Record<string, unknown> }>(
      `/api/visualizations/${id}`,
      body,
    );
  }

  async listVisualizations(queryId: number) {
    return this.fetch<{ id: number; type: string; name: string; options: Record<string, unknown> }[]>(
      `/api/queries/${queryId}/visualizations`,
    );
  }

  async deleteVisualization(id: number) {
    return this.del<void>(`/api/visualizations/${id}`);
  }

  async createDashboard(name: string) {
    return this.post<{ id: number; slug: string; name: string }>("/api/dashboards", { name });
  }

  async getDashboard(slug: string) {
    return this.fetch<{ id: number; slug: string; name: string; tags: string[] }>(
      `/api/dashboards/${slug}`,
    );
  }

  async updateDashboard(slug: string, body: { name?: string; tags?: string[] }) {
    return this.post<{ id: number; slug: string; name: string; tags: string[] }>(
      `/api/dashboards/${slug}`,
      body,
    );
  }

  async deleteDashboard(slug: string) {
    return this.del<void>(`/api/dashboards/${slug}`);
  }

  async publishDashboard(slug: string) {
    return this.post<{ id: number; published: boolean }>(`/api/dashboards/${slug}/publish`);
  }

  async discardDashboardChanges(slug: string) {
    return this.post<{ id: number; has_unpublished_changes: boolean }>(
      `/api/dashboards/${slug}/discard`,
    );
  }

  /** Batched, atomic position write — the endpoint a multi-tile drag lands as one request. */
  async saveDashboardLayout(
    slug: string,
    positions: { widget_id: number; position: { col: number; row: number; sizeX: number; sizeY: number } }[],
  ) {
    return this.fetch<{ id: number; options: Record<string, unknown> }[]>(
      `/api/dashboards/${slug}/layout`,
      { method: "PUT", body: JSON.stringify({ positions }) },
    );
  }

  async shareDashboard(slug: string) {
    return this.post<{ secret_token: string; public_url: string }>(
      `/api/dashboards/${slug}/share`,
      {},
    );
  }

  async revokeShareDashboard(slug: string) {
    return this.del<void>(`/api/dashboards/${slug}/share`);
  }

  async listDashboardTags() {
    return this.fetch<{ tags: string[] }>("/api/dashboards/tags");
  }

  /** Note: dashboard favorite/unfavorite is keyed by numeric dashboard id, not slug. */
  async starDashboard(id: number) {
    return this.post<void>(`/api/dashboards/${id}/favorite`, {});
  }

  async unstarDashboard(id: number) {
    return this.del<void>(`/api/dashboards/${id}/favorite`);
  }

  async createWidget(body: {
    dashboard_id: number;
    visualization_id: number | null;
    text: string | null;
    options: Record<string, unknown>;
  }) {
    return this.post<{ id: number; dashboard_id: number }>("/api/widgets", body);
  }

  async updateWidget(id: number, body: Record<string, unknown>) {
    return this.post<{ id: number }>(`/api/widgets/${id}`, body);
  }

  async deleteWidget(id: number) {
    return this.del<void>(`/api/widgets/${id}`);
  }

  async createMonitor(body: {
    query_id: number;
    name: string;
    options: {
      column: string;
      op: string;
      value: number;
      muted?: boolean;
      custom_subject?: string;
      custom_body?: string;
    };
    rearm?: number;
  }) {
    return this.post<{
      id: number;
      name: string;
      query: { id: number; name: string };
      options: Record<string, unknown>;
      state: string;
    }>("/api/monitors", body);
  }

  async getMonitor(id: number) {
    return this.fetch<{
      id: number;
      name: string;
      query: { id: number; name: string };
      options: Record<string, unknown>;
      state: string;
    }>(`/api/monitors/${id}`);
  }

  async deleteMonitor(id: number) {
    return this.del<void>(`/api/monitors/${id}`);
  }

  async muteMonitor(id: number) {
    return this.post<{
      id: number;
      options: Record<string, unknown>;
    }>(`/api/monitors/${id}/mute`, {});
  }

  async unmuteMonitor(id: number) {
    return this.post<{
      id: number;
      options: Record<string, unknown>;
    }>(`/api/monitors/${id}/unmute`, {});
  }

  async createMonitorSubscription(monitorId: number, body: { destination_id?: number | null }) {
    return this.post<{
      id: number;
      monitor_id: number;
      user: { id: number; email: string };
      destination: { id: number; name: string; type: string } | null;
    }>(`/api/monitors/${monitorId}/subscriptions`, body);
  }

  async listMonitorSubscriptions(monitorId: number) {
    return this.fetch<
      {
        id: number;
        monitor_id: number;
        user: { id: number; email: string };
        destination: { id: number; name: string; type: string } | null;
      }[]
    >(`/api/monitors/${monitorId}/subscriptions`);
  }

  async deleteMonitorSubscription(subId: number) {
    return this.del<void>(`/api/monitor_subscriptions/${subId}`);
  }

  async notifyMonitorSubscription(monitorId: number, subId: number) {
    return this.post<void>(`/api/monitors/${monitorId}/subscriptions/${subId}/notify`, {});
  }

  async createDestination(body: { name: string; type: string; options: Record<string, unknown> }) {
    return this.post<{ id: number; name: string; type: string; options: Record<string, unknown> }>(
      "/api/destinations",
      body,
    );
  }

  async updateDestination(id: number, body: { name?: string; options?: Record<string, unknown> }) {
    return this.post<{ id: number; name: string; type: string; options: Record<string, unknown> }>(
      `/api/destinations/${id}`,
      body,
    );
  }

  async deleteDestination(id: number) {
    return this.del<void>(`/api/destinations/${id}`);
  }

  async listDestinationTypes() {
    return this.fetch<{ name: string; type: string; configuration_schema: unknown }[]>(
      "/api/destinations/types",
    );
  }

  async createSnippet(body: { trigger: string; snippet: string; description?: string }) {
    return this.post<{ id: number; trigger: string; snippet: string }>(
      "/api/query_snippets",
      body,
    );
  }

  async updateSnippet(id: number, body: { trigger?: string; snippet?: string; description?: string }) {
    return this.post<{ id: number; trigger: string; snippet: string }>(
      `/api/query_snippets/${id}`,
      body,
    );
  }

  async deleteSnippet(id: number) {
    return this.del<void>(`/api/query_snippets/${id}`);
  }

  async getAiSettings() {
    return this.fetch<{ key_configured: boolean; key_source: string }>("/api/organization/ai");
  }

  async updateAiSettings(body: { gemini_api_key?: string; rate_limit_per_min?: number }) {
    return this.post<{ key_configured: boolean; key_source: string }>(
      "/api/organization/ai",
      body,
    );
  }

  rawFetch<T>(path: string, init?: RequestInit & { headers?: Headers }): Promise<T> {
    return this.fetch<T>(path, init);
  }

  get sessionCookieHeader(): string {
    return this.sessionCookie;
  }
}

export async function withTargetDs(
  api: E2EApiClient,
  fn: (dsId: number, dsName: string) => Promise<void>,
): Promise<void> {
  const name = `e2e-target-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
  const ds = await api.createDataSource({ name, type: "pg", options: TARGET_DS_OPTIONS });
  try {
    await fn(ds.id, name);
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
  }
}

/** Poll a job until SUCCESS (status 3); returns the query_result_id. */
export async function pollJob(api: E2EApiClient, jobId: string): Promise<number> {
  for (let i = 0; i < 30; i++) {
    const { job } = await api.rawFetch<{
      job: { status: number; query_result_id: number | null; error: string | null };
    }>(`/api/jobs/${jobId}`);
    if (job.status === 3 && job.query_result_id != null) return job.query_result_id;
    if (job.status === 4) throw new Error(`Job failed: ${job.error}`);
    if (job.status === 5) throw new Error("Job cancelled");
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error("Job did not complete within timeout");
}

export async function executeAndWait(
  api: E2EApiClient,
  queryId: number,
  params?: Record<string, unknown>,
): Promise<number> {
  const res = await api.executeQuery(queryId, params);
  if ("query_result" in res && res.query_result) {
    return (res.query_result as { id: number }).id;
  }
  if ("job" in res && res.job) {
    return pollJob(api, res.job!.id);
  }
  throw new Error("Unexpected execute response shape");
}

// Module-level cache per Playwright worker — avoids repeated logins hitting the rate limit.
let _adminClient: E2EApiClient | null = null;
let _adminClientPromise: Promise<E2EApiClient> | null = null;

export async function loginAsAdmin(): Promise<E2EApiClient> {
  if (_adminClient) return _adminClient;
  if (_adminClientPromise) return _adminClientPromise;
  const email = process.env.ADMIN_EMAIL ?? "admin@example.com";
  const password = process.env.ADMIN_PASSWORD ?? "adminpassword";
  _adminClientPromise = (async () => {
    const client = new E2EApiClient();
    await client.login(email, password);
    _adminClient = client;
    return client;
  })();
  return _adminClientPromise;
}
