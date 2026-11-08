import { Navigate, useSearchParams } from "react-router";
import { PageHeader } from "@/components/common/PageHeader";
import { PageTabs, type PageTab } from "@/components/common/PageTabs";
import { UsersPage } from "@/pages/UsersPage";
import GroupsPage from "@/pages/GroupsPage";
import RolesPage from "@/pages/RolesPage";
import { usePermissions } from "@/lib/permissions";
import { useDocumentTitle } from "@/lib/useDocumentTitle";

export const TABS = [
  { key: "users", label: "Users", anyOf: ["view_users", "manage_users", "admin"] },
  { key: "groups", label: "Groups", anyOf: ["view_groups", "manage_groups", "admin"] },
  { key: "roles", label: "Roles", anyOf: ["view_roles", "manage_roles", "admin"] },
] as const;

type TabKey = (typeof TABS)[number]["key"];

export default function AccessControlPage() {
  const [params] = useSearchParams();
  const { hasAny } = usePermissions();
  const rawTab = params.get("tab");

  const visibleTabs = TABS.filter((t) => hasAny(t.anyOf));
  const requestedTab = visibleTabs.find((t) => t.key === rawTab);
  // Falls back to "users" only when visibleTabs is empty (handled by the redirect below); keeps
  // `tab` non-optional.
  const tab: TabKey = requestedTab?.key ?? visibleTabs[0]?.key ?? "users";
  const activeLabel = visibleTabs.find((t) => t.key === tab)?.label ?? "Access Control";
  useDocumentTitle(`${activeLabel} · arcodash`);

  // RequirePermission guarantees a visible tab; the fallback just avoids a crash.
  if (visibleTabs.length === 0) {
    return <Navigate to="/settings" replace />;
  }

  // A stale/forbidden ?tab= redirects to the resolved tab rather than rendering one under a
  // mismatched URL.
  if (rawTab != null && requestedTab == null) {
    return <Navigate to={`/access?tab=${tab}`} replace />;
  }

  const pageTabs: PageTab[] = visibleTabs.map((t) => ({
    key: t.key,
    label: t.label,
    to: `/access?tab=${t.key}`,
    id: `access-tab-${t.key}`,
    ariaControls: `access-panel-${t.key}`,
  }));

  return (
    <div className="space-y-6">
      <PageHeader title="Access Control" />

      {visibleTabs.length >= 2 && <PageTabs tabs={pageTabs} activeKey={tab} />}

      <div role="tabpanel" id={`access-panel-${tab}`} aria-labelledby={`access-tab-${tab}`}>
        {tab === "users" && <UsersPage />}
        {tab === "groups" && <GroupsPage />}
        {tab === "roles" && <RolesPage />}
      </div>
    </div>
  );
}
