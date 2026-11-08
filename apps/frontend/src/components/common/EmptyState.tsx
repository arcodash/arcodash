import type { LucideIcon } from "lucide-react"
import type { ReactNode } from "react"

import { Card, CardContent } from "@/components/ui/card"

interface EmptyStateProps {
  icon: LucideIcon
  title: string
  description?: string
  action?: ReactNode
  iconClassName?: string // background + icon color, e.g. "bg-indigo-50 text-indigo-600" — icon color comes from `currentColor` via this text-color class
}

export function EmptyState({ icon: Icon, title, description, action, iconClassName }: EmptyStateProps) {
  return (
    <Card>
      <CardContent className="py-12 text-center">
        <div className={`mx-auto mb-4 flex h-12 w-12 items-center justify-center rounded-full ${iconClassName ?? "bg-muted text-muted-foreground"}`}>
          <Icon className="h-6 w-6" />
        </div>
        <p className="text-sm font-medium text-foreground">{title}</p>
        {description && <p className="mt-1 text-sm text-muted-foreground mx-auto max-w-[34ch]">{description}</p>}
        {action && <div className="mt-4 flex justify-center">{action}</div>}
      </CardContent>
    </Card>
  )
}
