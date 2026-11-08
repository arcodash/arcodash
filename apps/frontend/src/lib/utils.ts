import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

// Safari with cookies blocked, sandboxed iframes and some private modes throw SecurityError on any
// localStorage access; degrade instead of crashing the page.
export function readStorage(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function writeStorage(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
  }
}

// The Clipboard API is unavailable on non-secure origins (plain http), so a copy button must report failure rather than throw.
export async function copyToClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/** Case-insensitive substring match of search-box text against any of `fields`; blank text matches everything. */
export function matchesSearch(search: string, ...fields: string[]): boolean {
  const q = search.trim().toLowerCase();
  return fields.some((f) => f.toLowerCase().includes(q));
}
