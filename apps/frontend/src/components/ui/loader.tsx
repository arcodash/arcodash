import { useState, type ReactNode } from "react";
import { cn } from "@/lib/utils";

export type LoaderVariant = "pendulum" | "flip" | "radar" | "bounce" | "comet";

const LOADER_LINES: Record<LoaderVariant, [string, string]> = {
  pendulum: ["Swinging through the numbers.", "Back and forth, gathering your data."],
  flip: ["Flipping through your data.", "Turning it over, one side at a time."],
  radar: ["Scanning for your data.", "Pinging the database. Standby."],
  bounce: ["Bouncing the numbers into place.", "Waiting for the dot to land."],
  comet: ["Racing to your results.", "Chasing down the data."],
};

export const INLINE_POOL: Exclude<LoaderVariant, "flip">[] = ["pendulum", "radar", "bounce", "comet"];
const PAGE_POOL: LoaderVariant[] = ["pendulum", "flip", "radar", "bounce", "comet"];

const pick = <T,>(items: readonly T[]): T => items[Math.floor(Math.random() * items.length)];

/** The brand arc every non-flip variant draws its dot over. */
const ARC = {
  d: "M8 38 A16 16 0 0 1 40 38",
  fill: "none",
  stroke: "#7e14ff",
  strokeWidth: "7",
  strokeLinecap: "round",
} as const;

const ARC_MARKS: Record<Exclude<LoaderVariant, "flip">, ReactNode> = {
  pendulum: (
    <>
      <path className="ld-arc-rock" {...ARC} />
      <circle className="ld-dot-pulse" cx="24" cy="35" r="5.5" fill="#47bfff" />
    </>
  ),
  radar: (
    <>
      <path {...ARC} />
      <circle className="ld-ring" cx="24" cy="35" r="5.5" fill="none" stroke="#47bfff" strokeWidth="2" />
      <circle cx="24" cy="35" r="5.5" fill="#47bfff" />
    </>
  ),
  bounce: (
    <>
      <path {...ARC} />
      <ellipse className="ld-shadow" cx="24" cy="40" rx="5" ry="1.4" fill="#7e14ff" />
      <circle className="ld-dot-bounce" cx="24" cy="35" r="5" fill="#47bfff" />
    </>
  ),
  comet: (
    <>
      <path {...ARC} opacity=".25" />
      <path className="ld-comet" {...ARC} />
      <circle cx="24" cy="35" r="5.5" fill="#47bfff" />
    </>
  ),
};

function LoaderIcon({ variant, size }: { variant: LoaderVariant; size: number }) {
  if (variant === "flip") {
    return (
      <div style={{ perspective: 300 }}>
        <svg className="ld-flip" viewBox="0 0 48 48" width={size} height={size} aria-hidden="true">
          <rect width="48" height="48" rx="11" fill="#7e14ff" />
          <path d="M11 36.5 A13 13 0 0 1 37 36.5" fill="none" stroke="#ffffff" strokeWidth="6" strokeLinecap="round" />
          <circle cx="24" cy="34" r="5" fill="#47bfff" />
        </svg>
      </div>
    );
  }
  return (
    <svg viewBox="0 0 48 48" width={size} height={size} aria-hidden="true">
      {ARC_MARKS[variant]}
    </svg>
  );
}

interface LoaderProps {
  size?: number;
  label?: string | boolean;
  variant?: LoaderVariant;
  className?: string;
  /** Set when this loader has no surrounding text of its own (e.g. a lone spinner in an empty
   * panel) and must announce its status to screen readers itself. Leave unset when it sits next
   * to or inside other labelled content (a button, a status cell) — otherwise its label gets
   * absorbed into that content's accessible name. */
  announce?: boolean;
}

export function Loader({ size = 20, label, variant, className, announce = false }: LoaderProps) {
  const [v] = useState<LoaderVariant>(() => variant ?? pick(INLINE_POOL));

  const text = label === true ? "Loading…" : label || undefined;

  return (
    <span
      {...(announce ? { role: "status" as const, "aria-label": typeof label === "string" ? label : "Loading" } : {})}
      className={cn("inline-flex items-center gap-2", className)}
    >
      <LoaderIcon variant={v} size={size} />
      {text && <span>{text}</span>}
    </span>
  );
}

interface PageLoaderProps {
  variant?: LoaderVariant;
  line?: string;
  className?: string;
}

export function PageLoader({ variant, line, className }: PageLoaderProps) {
  const [[v, l]] = useState<[LoaderVariant, string]>(() => {
    const picked = variant ?? pick(PAGE_POOL);
    return [picked, line ?? pick(LOADER_LINES[picked])];
  });

  return (
    <div
      role="status"
      className={cn(
        "flex min-h-[60vh] w-full flex-col items-center justify-center animate-fade-in",
        className,
      )}
    >
      <LoaderIcon variant={v} size={64} />
      <p className="mt-4 max-w-xs text-center text-sm text-muted-foreground">{l}</p>
    </div>
  );
}
