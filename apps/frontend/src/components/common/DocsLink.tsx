import { ExternalLink } from "lucide-react";
import { docsUrl, type DocsTopic } from "@/lib/docs";

/** The one contextual "go read the docs" affordance used across the dashboard — same visible
 * label everywhere ("Learn more") so it never competes with real in-product actions; the topic
 * travels in an sr-only span so screen-reader users still get a distinct name per link. */
export function DocsLink({ topic, label }: { topic: DocsTopic; label: string }) {
  return (
    <a
      href={docsUrl(topic)}
      target="_blank"
      rel="noreferrer"
      className="inline-flex items-center gap-1 text-xs text-muted-foreground underline decoration-dotted underline-offset-2 hover:text-foreground"
    >
      Learn more
      <ExternalLink className="size-3" aria-hidden="true" />
      <span className="sr-only"> about {label} (opens in a new tab)</span>
    </a>
  );
}
