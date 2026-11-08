const HREF =
  "https://www.arcodash.com/?utm_source=arcodash-app&utm_medium=public-share&utm_content=powered-by";

export function PoweredBy({ className = "" }: { className?: string }) {
  return (
    <span className={`text-xs text-muted-foreground ${className}`.trim()}>
      Powered by{" "}
      <a href={HREF} target="_blank" rel="noopener" className="underline-offset-2 hover:underline">
        arcodash
      </a>
    </span>
  );
}
