/** Share of a prompt the server re-read from its cache; nothing when it did not say. */
export function hitRate(cached: number | undefined, measured: number): string | undefined {
  if (cached === undefined || measured <= 0) return undefined;
  const hits = Math.min(cached, measured);
  const percent = (100 * hits) / measured;
  // Rounding up to 100 would claim a full hit that did not happen.
  return hits < measured && percent >= 99.5 ? ">99%" : `${Math.round(percent)}%`;
}
