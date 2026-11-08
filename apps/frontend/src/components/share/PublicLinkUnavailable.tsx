import { PoweredBy } from "./PoweredBy";

export function PublicLinkUnavailable() {
  return (
    <div className="flex h-dvh flex-col items-center justify-center gap-3 bg-background px-4 text-center">
      <h1 className="text-2xl font-bold text-foreground">Link unavailable</h1>
      <p className="max-w-md text-sm text-muted-foreground">
        This shared view doesn&apos;t exist or is no longer available. Ask the person who
        shared it for a new link.
      </p>
      <PoweredBy className="mt-6" />
    </div>
  );
}
