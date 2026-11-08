/** Model text is rendered as text only; backtick-quoted runs get a mono face. */
export function RichText({ text }: { text: string }) {
  return (
    <>
      {text.split("`").map((seg, i) => (
        <span key={i} className={i % 2 === 1 ? "font-mono" : undefined}>
          {seg}
        </span>
      ))}
    </>
  );
}
