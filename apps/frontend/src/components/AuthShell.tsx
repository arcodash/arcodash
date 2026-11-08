import type { ReactNode } from "react";
import { Link } from "react-router";
import { Wordmark } from "@/components/AppShell";
import { extensions } from "@/extensions";

/** Wraps every unauthenticated auth page (login, forgot/reset password, invitation accept) so
 * all of them inherit whatever the deployment supplies. Renders `children` in a plain centered
 * layout unless an extension supplies `authShellSlot`. */
export function AuthShell({ children }: { children: ReactNode }) {
  const Slot = extensions.find((e) => e.authShellSlot)?.authShellSlot;
  if (Slot) return <Slot>{children}</Slot>;
  return (
    <div className="flex min-h-[70vh] flex-col items-center justify-center gap-6">
      <Link to="/" className="flex items-center gap-2">
        <img src="/favicon.svg" alt="" width={32} height={32} aria-hidden="true" className="shrink-0" />
        <Wordmark className="text-lg font-semibold tracking-wide text-foreground" />
      </Link>
      {children}
    </div>
  );
}
