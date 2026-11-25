import { extensions } from "@/extensions";

export function hasResultFootnote(): boolean {
  return extensions.some((ext) => ext.resultFootnoteSlot != null);
}

interface Props {
  retrievedAt?: string;
  placement: "editor" | "widget";
  publicView?: boolean;
}

export function ResultFootnote(props: Props) {
  return (
    <>
      {extensions.map((ext, i) => (ext.resultFootnoteSlot ? <ext.resultFootnoteSlot key={i} {...props} /> : null))}
    </>
  );
}
