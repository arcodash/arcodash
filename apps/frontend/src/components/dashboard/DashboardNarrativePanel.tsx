import { useEffect, useRef, useState } from "react";
import { Link } from "react-router";
import ReactMarkdown from "react-markdown";
import { ChevronDown, ChevronUp, Crosshair, Loader2, RefreshCw, Sparkles } from "lucide-react";
import { Button } from "@/components/ui/button";
import { timeAgo, exactTs } from "@/lib/scheduling";
import { widgetTitle } from "./WidgetCard";
import type { DashboardNarrative, Widget } from "@/types/api";

const HIGHLIGHT_MS = 2000;
const STALE_MS = 24 * 3600 * 1000;
const RING = ["ring-2", "ring-primary", "ring-offset-2"];

export interface NarrativeError {
  status: string;
  message: string;
}

interface Props {
  widgets: Widget[];
  narrative: DashboardNarrative | null;
  expanded: boolean;
  onToggle: () => void;
  /** A generation is in flight. */
  pending: boolean;
  error: NarrativeError | null;
  onRegenerate: () => void;
  isAdmin: boolean;
}

function labelFor(c: DashboardNarrative["confidence"]) {
  return `${c[0].toUpperCase()}${c.slice(1)} confidence`;
}

const WIDGET_HREF = /^#widget-(\d+)$/;
const MD_ELEMENTS = ["p", "strong", "em", "ul", "ol", "li", "code", "br", "a"];
const mdUrl = (url: string) => (WIDGET_HREF.test(url) ? url : "");

function jumpToWidget(id: number) {
  const el = document.querySelector<HTMLElement>(`[data-widget-id="${id}"]`);
  if (!el) return;
  const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;
  el.scrollIntoView({ block: "center", behavior: reduced ? "auto" : "smooth" });
  el.focus({ preventScroll: true });
  el.classList.add(...RING);
  window.setTimeout(() => el.classList.remove(...RING), HIGHLIGHT_MS);
}

export function DashboardNarrativePanel({
  widgets,
  narrative,
  expanded,
  onToggle,
  pending,
  error,
  onRegenerate,
  isAdmin,
}: Props) {
  const [now] = useState(() => Date.now());
  const statusText = pending ? "Updating…" : error ? error.message : narrative ? "Summary ready." : "";
  // Announce "Summary ready." only for a generation completed while mounted, not the initial GET.
  const wasPending = useRef(false);
  const [announceReady, setAnnounceReady] = useState(false);
  useEffect(() => {
    if (pending) wasPending.current = true;
    else if (wasPending.current) {
      wasPending.current = false;
      setAnnounceReady(true);
    }
  }, [pending]);

  const created = narrative ? new Date(narrative.created_at).getTime() : null;
  const stale = created != null && now - created > STALE_MS;
  const nonText = widgets.filter((w) => w.visualization_id != null);
  const byId = new Map(widgets.map((w) => [w.id, w]));

  const adminLink =
    isAdmin && error && (error.status === "disabled" || error.status === "not_configured" || error.status === "invalid_key") ? (
      <Link to="/organization/ai" className="ml-2 underline">
        {error.status === "disabled" ? "Turn on in AI Settings" : "Open AI Settings"}
      </Link>
    ) : null;

  const regenerate = (
    <Button type="button" size="sm" variant="ghost" onClick={onRegenerate} disabled={pending}>
      {pending ? <Loader2 className="mr-1 h-3.5 w-3.5 animate-spin" /> : <RefreshCw className="mr-1 h-3.5 w-3.5" />}
      {pending ? "Regenerating…" : "Regenerate"}
    </Button>
  );

  const time = created != null && (
    <time className="ml-auto text-xs text-muted-foreground" title={exactTs(created)}>
      {timeAgo(created, now)}
    </time>
  );

  const showError = error != null && !pending;
  const firstRun = pending && narrative == null;

  return (
    <section aria-labelledby="dash-narrative-heading" className="rounded-md border border-border p-3">
      <div role="status" aria-live="polite" className="sr-only">
        {pending || error || announceReady ? statusText : ""}
      </div>
      {narrative && !expanded ? (
        <button
          type="button"
          aria-expanded={false}
          onClick={onToggle}
          className="flex w-full min-w-0 cursor-pointer flex-col gap-1 text-left"
        >
          <span className="flex w-full items-center gap-2">
            <Sparkles className="h-4 w-4 shrink-0" aria-hidden />
            <h2 id="dash-narrative-heading" className="text-sm font-medium">AI summary</h2>
            {time}
            <span className="sr-only">Show summary</span>
            <ChevronDown className="h-4 w-4 shrink-0" aria-hidden />
          </span>
          <span className="line-clamp-1 text-sm text-muted-foreground">{narrative.summary}</span>
        </button>
      ) : (
        <div id="dash-narrative-body" className="space-y-2">
          <div className="flex items-center gap-2">
            <Sparkles className="h-4 w-4 shrink-0" aria-hidden />
            <h2 id="dash-narrative-heading" className="text-sm font-medium">AI summary</h2>
            {time && (
              <>
                {time}
                <button
                  type="button"
                  aria-expanded
                  aria-controls="dash-narrative-body"
                  aria-label="Hide summary"
                  onClick={onToggle}
                  className="cursor-pointer rounded p-1 hover:bg-muted"
                >
                  <ChevronUp className="h-4 w-4" aria-hidden />
                </button>
              </>
            )}
          </div>

          {showError && (
            <p className="text-sm text-destructive">
              {error.message}
              {adminLink}
              {error.status === "generation_failed" && (
                <Button type="button" size="sm" variant="outline" className="ml-2" onClick={onRegenerate}>
                  Try again
                </Button>
              )}
            </p>
          )}

          {firstRun && (
            <div aria-busy="true" className="space-y-2">
              <p className="text-sm text-muted-foreground">
                Reading {nonText.length} {nonText.length === 1 ? "widget" : "widgets"}…
              </p>
              {[0, 1, 2].map((i) => (
                <div key={i} className="h-3 animate-pulse rounded bg-muted" />
              ))}
            </div>
          )}

          {narrative && (
            <>
              <div className="flex flex-wrap gap-x-2 text-xs text-muted-foreground">
                <span>{labelFor(narrative.confidence)}</span>
                {narrative.source === "digest" && <span>· From your morning brief</span>}
                {pending && (
                  <span className="inline-flex items-center gap-1">
                    <Loader2 className="h-3 w-3 animate-spin" aria-hidden /> Updating…
                  </span>
                )}
              </div>
              {narrative.confidence === "low" && (
                <p className="text-xs text-warning-foreground">
                  Low confidence — check the widgets before acting on this.
                </p>
              )}
              {stale && (
                <p className="text-xs text-muted-foreground">
                  This summary is from {timeAgo(created!, now)}. Widgets may have changed.{" "}
                  <button type="button" className="underline" onClick={onRegenerate} disabled={pending}>
                    Regenerate
                  </button>
                </p>
              )}
              {narrative.body ? (
                <div className="max-w-prose space-y-2 break-words text-sm leading-relaxed">
                  <ReactMarkdown
                    allowedElements={MD_ELEMENTS}
                    skipHtml
                    unwrapDisallowed
                    urlTransform={mdUrl}
                    components={{
                      ul: ({ node: _n, ...p }) => <ul className="list-disc space-y-1 pl-5" {...p} />,
                      ol: ({ node: _n, ...p }) => <ol className="list-decimal pl-5" {...p} />,
                      a: ({ href, children }) => {
                        const id = Number(WIDGET_HREF.exec(href ?? "")?.[1]);
                        if (!id || !byId.has(id)) return <>{children}</>;
                        return (
                          <button
                            type="button"
                            title={`Show widget ${widgetTitle(byId.get(id)!)}`}
                            onClick={() => jumpToWidget(id)}
                            className="cursor-pointer rounded text-primary underline underline-offset-2 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                          >
                            {children}
                          </button>
                        );
                      },
                    }}
                  >
                    {narrative.body}
                  </ReactMarkdown>
                </div>
              ) : (
                <p className="whitespace-pre-line break-words text-sm">{narrative.summary}</p>
              )}
              {!narrative.body && narrative.highlights.length > 0 && (
                <ul className="space-y-0.5">
                  {narrative.highlights.map((h, i) => {
                    const w = byId.get(h.widget_id);
                    if (!w) {
                      return (
                        <li key={i} className="px-2 py-1 text-sm">
                          {h.text}
                        </li>
                      );
                    }
                    const title = widgetTitle(w);
                    return (
                      <li key={i}>
                        <button
                          type="button"
                          aria-label={`Show widget ${title}: ${h.text}`}
                          onClick={() => jumpToWidget(h.widget_id)}
                          className="flex w-full cursor-pointer items-start gap-1.5 rounded px-2 py-1 text-left text-sm hover:bg-muted focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                        >
                          <Crosshair className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
                          <span className="min-w-0 break-words">
                            {h.text}
                            <span className="text-muted-foreground"> · {title}</span>
                          </span>
                        </button>
                      </li>
                    );
                  })}
                </ul>
              )}
              <div className="flex flex-wrap items-center justify-between gap-x-2 text-xs text-muted-foreground">
                <p>
                  Written by {narrative.model} from each widget&apos;s latest results. Widget results are sent to
                  your AI provider.
                </p>
                {regenerate}
              </div>
            </>
          )}
        </div>
      )}
    </section>
  );
}
