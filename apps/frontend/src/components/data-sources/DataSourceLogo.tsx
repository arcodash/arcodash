import { useState } from "react";
import { getDataSourceMeta } from "@/lib/dataSourceMeta";

/** The type's logo in a muted tile, falling back to its category icon if the image fails to load. */
export function DataSourceLogo({ type, alt }: { type: string; alt: string }) {
  const [failed, setFailed] = useState(false);
  const { icon: Icon, logoUrl, color } = getDataSourceMeta(type);
  return (
    <div className="p-2 rounded-lg bg-muted flex items-center justify-center">
      {logoUrl && !failed ? (
        <img src={logoUrl} alt={alt} className="h-8 w-8 object-contain" onError={() => setFailed(true)} />
      ) : (
        <Icon className="h-5 w-5" style={color ? { color } : undefined} />
      )}
    </div>
  );
}
