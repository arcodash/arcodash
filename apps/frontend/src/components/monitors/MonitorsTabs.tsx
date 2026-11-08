import { useLocation } from "react-router";
import { PageTabs, type PageTab } from "@/components/common/PageTabs";
import { usePermissions } from "@/lib/permissions";

/** Tab strip shared by MonitorsPage and DestinationsPage — destinations live under the
 * Monitors area since alerts are their only purpose, but the list itself is org-wide. */
export function MonitorsTabs() {
  const { has, hasAny } = usePermissions();
  const { pathname } = useLocation();

  const tabs: PageTab[] = [];
  if (has("view_monitor")) tabs.push({ key: "monitors", label: "Monitors", to: "/monitors" });
  if (hasAny(["view_destinations", "manage_destinations"])) {
    tabs.push({ key: "destinations", label: "Destinations", to: "/destinations" });
  }

  const activeKey = tabs.find((t) => t.to === pathname)?.key ?? "";
  return <PageTabs tabs={tabs} activeKey={activeKey} />;
}
