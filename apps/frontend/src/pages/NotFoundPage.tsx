import { Link } from "react-router";
import { Button } from "@/components/ui/button";
import { useDocumentTitle } from "@/lib/useDocumentTitle";

export function NotFoundPage() {
  useDocumentTitle("Page not found · arcodash");
  return (
    <div className="flex min-h-[70vh] flex-col items-center justify-center gap-3 text-center">
      {/* Same h1 treatment as PageHeader's title — 404 has no toolbar to sit beside, so it
          doesn't use the PageHeader component itself, just its typography. */}
      <h1 className="text-2xl font-bold text-foreground">404</h1>
      <p className="text-sm text-muted-foreground">This page doesn't exist.</p>
      <Button asChild>
        <Link to="/">Back to home</Link>
      </Button>
    </div>
  );
}
