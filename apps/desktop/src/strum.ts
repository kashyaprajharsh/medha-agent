/** Lanes (ascending) a pointer passed moving from `from` to `to`, in the order it met them. */
export function crossed(from: number, to: number, lanes: readonly number[]): number[] {
  if (to > from) return lanes.flatMap((lane, index) => (from < lane && lane <= to ? [index] : []));
  return lanes
    .flatMap((lane, index) => (to <= lane && lane < from ? [index] : []))
    .reverse();
}

/** A released string: full displacement at t = 0, swinging at `rate` Hz while it decays. */
export function ring(amp: number, seconds: number, rate: number, decay: number) {
  const envelope = amp * Math.exp(-seconds / decay);
  return { envelope, offset: envelope * Math.cos(2 * Math.PI * rate * seconds) };
}
