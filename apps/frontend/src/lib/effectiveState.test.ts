import { describe, it, expect } from "vitest";
import { effectiveState } from "./effectiveState";
import type { Query } from "@/types/api";

function q(overrides: Partial<Query>): Parameters<typeof effectiveState>[0] {
  return {
    published: false,
    has_unpublished_changes: false,
    visibility: "workspace",
    ...overrides,
  };
}

describe("effectiveState", () => {
  const cases: Array<{
    name: string;
    query: Partial<Query>;
    role: "editor" | "viewer";
    badge: "Draft" | "Published";
    lifecycle: "draft" | "published_clean" | "published_pending";
    hint: string;
    pending: boolean;
  }> = [
    {
      name: "draft, workspace, editor",
      query: { published: false, visibility: "workspace" },
      role: "editor",
      badge: "Draft",
      lifecycle: "draft",
      hint: "Only you and people you've shared with can see this. Publishing makes it visible to your workspace.",
      pending: false,
    },
    {
      name: "draft, restricted, editor",
      query: { published: false, visibility: "restricted" },
      role: "editor",
      badge: "Draft",
      lifecycle: "draft",
      hint: "Only you and people you've shared with can see this. It stays restricted after publishing.",
      pending: false,
    },
    {
      name: "published clean, workspace, editor",
      query: { published: true, has_unpublished_changes: false, visibility: "workspace" },
      role: "editor",
      badge: "Published",
      lifecycle: "published_clean",
      hint: "Visible to everyone in your workspace.",
      pending: false,
    },
    {
      name: "published clean, restricted, editor",
      query: { published: true, has_unpublished_changes: false, visibility: "restricted" },
      role: "editor",
      badge: "Published",
      lifecycle: "published_clean",
      hint: "Only you and people you've shared with can see this.",
      pending: false,
    },
    {
      name: "published pending, workspace, editor",
      query: { published: true, has_unpublished_changes: true, visibility: "workspace" },
      role: "editor",
      badge: "Published",
      lifecycle: "published_pending",
      hint: "Your workspace sees the published version. Your edits go live when you publish them.",
      pending: true,
    },
    {
      name: "published pending, restricted, editor",
      query: { published: true, has_unpublished_changes: true, visibility: "restricted" },
      role: "editor",
      badge: "Published",
      lifecycle: "published_pending",
      hint: "The people you've shared with see the published version. Your edits go live when you publish them.",
      pending: true,
    },
    {
      name: "published pending, workspace, viewer never sees pending",
      query: { published: true, has_unpublished_changes: true, visibility: "workspace" },
      role: "viewer",
      badge: "Published",
      lifecycle: "published_pending",
      hint: "Visible to everyone in your workspace.",
      pending: false,
    },
    {
      name: "published pending, restricted, viewer never sees pending",
      query: { published: true, has_unpublished_changes: true, visibility: "restricted" },
      role: "viewer",
      badge: "Published",
      lifecycle: "published_pending",
      hint: "Only you and people you've shared with can see this.",
      pending: false,
    },
    {
      name: "published clean, workspace, viewer",
      query: { published: true, has_unpublished_changes: false, visibility: "workspace" },
      role: "viewer",
      badge: "Published",
      lifecycle: "published_clean",
      hint: "Visible to everyone in your workspace.",
      pending: false,
    },
    {
      name: "draft, workspace, viewer (unreachable in practice, but pure function must not crash)",
      query: { published: false, visibility: "workspace" },
      role: "viewer",
      badge: "Draft",
      lifecycle: "draft",
      hint: "Only you and people you've shared with can see this. Publishing makes it visible to your workspace.",
      pending: false,
    },
  ];

  it.each(cases)("$name", ({ query, role, badge, lifecycle, hint, pending }) => {
    const result = effectiveState(q(query), role);
    expect(result.badge).toBe(badge);
    expect(result.lifecycle).toBe(lifecycle);
    expect(result.hint).toBe(hint);
    expect(result.pending).toBe(pending);
  });

  // Dashboards share the identical lifecycle shape — one function serves both, with no
  // object-type branch, so a plain `{published, has_unpublished_changes, visibility}` literal
  // (not a `Query`) must type-check and classify identically.
  it.each(cases)("dashboard-shaped input: $name", ({ query, role, badge, lifecycle, pending }) => {
    const dashboardLike = {
      published: false,
      has_unpublished_changes: false,
      visibility: "workspace" as const,
      ...query,
    };
    const result = effectiveState(dashboardLike, role);
    expect(result.badge).toBe(badge);
    expect(result.lifecycle).toBe(lifecycle);
    expect(result.pending).toBe(pending);
  });
});
