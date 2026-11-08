import { useLocation } from "react-router";
import { PageTabs, type PageTab } from "@/components/common/PageTabs";

const TABS: PageTab[] = [
  { key: "providers", label: "Providers", to: "/organization/ai" },
  { key: "usage", label: "Usage", to: "/organization/ai/usage" },
];

/** Tab strip shared by AiSettingsPage (Providers) and AiUsagePage (Usage) — both routes are
 * already admin-gated, so unlike MonitorsTabs this needs no permission checks. */
export function AiTabs() {
  const { pathname } = useLocation();
  const activeKey = TABS.find((t) => t.to === pathname)?.key ?? "providers";
  return <PageTabs tabs={TABS} activeKey={activeKey} />;
}
