import { Link } from "react-router";
import {
  UsersRound,
  Building2,
  Sparkles,
  Activity,
  ScrollText,
  Bell,
  User,
} from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { PageHeader } from "@/components/common/PageHeader";
import { ACCESS_KEYS, usePermissions } from "@/lib/permissions";
import { extensions } from "@/extensions";

const PERSONAL_ITEMS = [
  { to: "/account", label: "Account", icon: User,
    description: "Your name, email, and password." },
  { to: "/settings/notifications", label: "Notifications", icon: Bell,
    description: "Get a daily digest email of the monitors you follow." },
];

export const WORKSPACE_ITEMS = [
  { to: "/organization", label: "Organization", icon: Building2, description: "Organization name and settings", anyOf: ["admin"] },
  { to: "/organization/ai", label: "AI Settings", icon: Sparkles, description: "AI providers, models, and usage limits", anyOf: ["admin"] },
  { to: "/access", label: "Access Control", icon: UsersRound, description: "Users, groups, and roles", anyOf: [...ACCESS_KEYS] },
];

export const SYSTEM_ITEMS = [
  { to: "/admin/status", label: "System Status", icon: Activity, description: "Queue depth and recent job activity", anyOf: ["admin"] },
  { to: "/admin/audit", label: "Audit Log", icon: ScrollText, description: "Review recent account and system activity", anyOf: ["admin"] },
];

interface SettingsCardProps {
  to: string;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
  description: string;
}

function SettingsCard({ to, label, icon: Icon, description }: SettingsCardProps) {
  return (
    <Link to={to}>
      <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
        <CardHeader className="pb-3">
          <div className="flex items-center gap-3">
            <div className="flex items-center justify-center rounded-lg bg-muted p-2">
              <Icon className="h-5 w-5" />
            </div>
            <CardTitle className="text-base">{label}</CardTitle>
          </div>
        </CardHeader>
        <CardContent className="text-sm text-muted-foreground">{description}</CardContent>
      </Card>
    </Link>
  );
}

function SettingsSection({ title, items }: { title: string; items: SettingsCardProps[] }) {
  if (items.length === 0) return null;
  return (
    <div className="space-y-3">
      <h2 className="text-sm font-medium text-muted-foreground">{title}</h2>
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-3">
        {items.map((item) => (
          <SettingsCard key={item.to} {...item} />
        ))}
      </div>
    </div>
  );
}

export function SettingsPage() {
  const { hasAny } = usePermissions();
  const workspaceItems = [
    ...WORKSPACE_ITEMS,
    ...extensions.flatMap((e) => e.settingsWorkspaceItems ?? []),
  ].filter((item) => hasAny(item.anyOf));
  const systemItems = SYSTEM_ITEMS.filter((item) => hasAny(item.anyOf));

  return (
    <div className="space-y-6">
      <PageHeader title="Settings" />
      <SettingsSection title="Personal" items={PERSONAL_ITEMS} />
      <SettingsSection title="Workspace" items={workspaceItems} />
      <SettingsSection title="System" items={systemItems} />
    </div>
  );
}
