import type { Visibility } from "@/types/api";

export type Lifecycle = "draft" | "published_clean" | "published_pending";
export type ViewerRole = "editor" | "viewer";

export interface EffectiveState {
  lifecycle: Lifecycle;
  badge: "Draft" | "Published";
  hint: string;
  /** Render the amber "Unpublished changes" badge. Editors only: a viewer can't tell the owner is
   * mid-revision. */
  pending: boolean;
}

const HINTS: Record<Lifecycle, { workspace: string; restricted: string }> = {
  draft: {
    workspace:
      "Only you and people you've shared with can see this. Publishing makes it visible to your workspace.",
    restricted:
      "Only you and people you've shared with can see this. It stays restricted after publishing.",
  },
  published_clean: {
    workspace: "Visible to everyone in your workspace.",
    restricted: "Only you and people you've shared with can see this.",
  },
  published_pending: {
    workspace: "Your workspace sees the published version. Your edits go live when you publish them.",
    restricted:
      "The people you've shared with see the published version. Your edits go live when you publish them.",
  },
};

/** Structural, not tied to `Query`: a query and a dashboard share one lifecycle shape, so one
 * function classifies both. */
interface LifecycleSubject {
  published: boolean;
  has_unpublished_changes: boolean;
  visibility: Visibility;
}

export function effectiveState(query: LifecycleSubject, viewerRole: ViewerRole): EffectiveState {
  const lifecycle: Lifecycle = !query.published
    ? "draft"
    : query.has_unpublished_changes
      ? "published_pending"
      : "published_clean";

  // A viewer's hint describes the published snapshot, never the owner's in-progress edit, so
  // pending downgrades to clean for hints.
  const hintLifecycle: Lifecycle =
    viewerRole === "viewer" && lifecycle === "published_pending" ? "published_clean" : lifecycle;
  const audience = query.visibility === "restricted" ? "restricted" : "workspace";

  return {
    lifecycle,
    badge: lifecycle === "draft" ? "Draft" : "Published",
    hint: HINTS[hintLifecycle][audience],
    pending: lifecycle === "published_pending" && viewerRole === "editor",
  };
}

/** Publish-confirmation copy: a first publish names who will see it; a republish says viewers get the update. */
export function publishDialogCopy(noun: "query" | "dashboard", visibility: Visibility, isFirstPublish: boolean) {
  if (!isFirstPublish) {
    return {
      title: "Publish your changes?",
      body: `Everyone who can see this ${noun} will get the updated version.`,
      confirmLabel: "Publish changes",
    };
  }
  return {
    title: `Publish this ${noun}?`,
    body:
      visibility === "restricted"
        ? "It will be visible to the people and groups you've shared with."
        : "It will be visible to everyone in your workspace.",
    confirmLabel: "Publish",
  };
}
