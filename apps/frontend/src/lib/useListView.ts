import { useState } from "react";
import { useMediaQuery } from "@/lib/useMediaQuery";
import { readStorage, writeStorage } from "@/lib/utils";

export type ListView = "table" | "cards";

function readStored(key: string): ListView {
  return readStorage(key) === "cards" ? "cards" : "table";
}

/** Table/cards preference per list in localStorage (default table). Below `md` the view is forced
 * to cards, but the stored preference is untouched. */
export function useListView(storageKey: string) {
  const [stored, setStored] = useState<ListView>(() => readStored(storageKey));
  const isDesktop = useMediaQuery("(min-width: 768px)");

  function setView(v: ListView) {
    setStored(v);
    writeStorage(storageKey, v);
  }

  return {
    view: isDesktop ? stored : "cards",
    storedView: stored,
    setView,
    isMobile: !isDesktop,
  } as const;
}
