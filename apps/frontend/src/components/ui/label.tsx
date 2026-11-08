import * as React from "react"
import { Label as LabelPrimitive } from "radix-ui"

import { cn } from "@/lib/utils"

function Label({
  className,
  ...props
}: React.ComponentProps<typeof LabelPrimitive.Root>) {
  return (
    <LabelPrimitive.Root
      data-slot="label"
      className={cn(
        "flex items-center gap-2 text-sm leading-none font-medium select-none group-data-[disabled=true]:pointer-events-none group-data-[disabled=true]:opacity-50 peer-disabled:cursor-not-allowed peer-disabled:opacity-50",
        className
      )}
      {...props}
    />
  )
}

/** The sr-only text is what a screen reader announces; a bare "*" is often skipped. */
function RequiredMark() {
  return (
    <>
      <span className="text-destructive" aria-hidden="true">
        {" "}
        *
      </span>
      <span className="sr-only"> (required)</span>
    </>
  )
}

function FieldError({ id, message }: { id: string; message?: string }) {
  if (!message) return null
  return (
    <p id={id} className="mt-1 text-xs text-destructive">
      {message}
    </p>
  )
}

export { Label, RequiredMark, FieldError }
