/** CSS font-size that shrinks with the container's inline size so `text` fits on one line.
 * Assumes tabular digits (~0.65em wide); only valid inside a `container-type:inline-size` parent. */
export function fitFontSize(text: string, maxRem: number, minRem = 0.875): string {
  const chars = Math.max(text.length, 1);
  return `clamp(${minRem}rem, calc(100cqw / ${(chars * 0.65).toFixed(2)}), ${maxRem}rem)`;
}
