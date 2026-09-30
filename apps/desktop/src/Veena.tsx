// A Saraswati veena seen from above: kudam with inlaid soundboard, fretted dandi,
// hanging sorakkai and a yali scroll. The strings are live — a pointer crossing one plucks it.
import { useEffect, useId, useRef } from "react";
import { crossed, ring } from "./strum";

const TAIL = 14;
const BRIDGE = 58;
const NUT = 300;
const NECK = { top: [37, 39.5], bottom: [53, 49.5] } as const;
// Twelve-tone spacing from the nut, as on the instrument: frets close up toward the bridge.
const FRETS = Array.from({ length: 24 }, (_, n) => NUT - (NUT - BRIDGE) * (1 - 2 ** (-(n + 1) / 12)));
// Heavier strings sit lower, swing slower and ring longer.
const STRINGS = [
  { tail: 44, bridge: 41, nut: 41.8, width: 0.5, rate: 11, decay: 0.5 },
  { tail: 45, bridge: 43.8, nut: 43.6, width: 0.6, rate: 9.5, decay: 0.6 },
  { tail: 46, bridge: 46.6, nut: 45.4, width: 0.72, rate: 8, decay: 0.7 },
  { tail: 47, bridge: 49.4, nut: 47.2, width: 0.85, rate: 6.5, decay: 0.8 },
] as const;
// Peak swing, in drawing units: enough to read at real size, short of tangling.
const SWING = 2.2;
// Where a pointer sweeping over the whole drawing meets each string.
const LANES = [22, 36, 50, 64];
const ROSETTE = Array.from({ length: 10 }, (_, i) => (i / 10) * 2 * Math.PI);

const edge = (side: readonly [number, number], x: number) =>
  side[0] + ((side[1] - side[0]) * (x - 86)) / (NUT - 86);
const along = (s: (typeof STRINGS)[number], x: number) =>
  s.bridge + ((s.nut - s.bridge) * (x - BRIDGE)) / (NUT - BRIDGE);
const bow = (s: (typeof STRINGS)[number], x: number, offset: number) =>
  `M${BRIDGE} ${s.bridge}Q${x.toFixed(2)} ${(along(s, x) + 2 * offset).toFixed(3)} ${NUT} ${s.nut}`;
const lens = (s: (typeof STRINGS)[number], x: number, spread: number) =>
  `${bow(s, x, spread)}Q${x.toFixed(2)} ${(along(s, x) - 2 * spread).toFixed(3)} ${BRIDGE} ${s.bridge}Z`;

type Pluck = { at: number; x: number; amp: number };

export function Veena() {
  const root = useRef<SVGSVGElement>(null);
  const wood = `veena-wood-${useId()}`;
  const glow = `veena-glow-${useId()}`;

  useEffect(() => {
    const svg = root.current;
    if (!svg || matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const lines = [...svg.querySelectorAll<SVGPathElement>(".v-string")];
    const rings = [...svg.querySelectorAll<SVGPathElement>(".v-ring")];
    const warmth = svg.querySelector<SVGPathElement>(".v-warmth");
    const plucks: (Pluck | null)[] = STRINGS.map(() => null);
    const timers: number[] = [];
    let frame = 0;
    let last: { y: number; t: number } | null = null;

    const draw = (now: number) => {
      frame = 0;
      let energy = 0;
      STRINGS.forEach((string, index) => {
        const pluck = plucks[index];
        if (!pluck) return;
        const { envelope, offset } = ring(pluck.amp, (now - pluck.at) / 1000, string.rate, string.decay);
        const done = envelope < 0.01;
        if (done) plucks[index] = null;
        lines[index].setAttribute("d", bow(string, pluck.x, done ? 0 : offset * SWING));
        rings[index].setAttribute("d", lens(string, pluck.x, done ? 0 : envelope * SWING));
        rings[index].style.opacity = done ? "0" : String(Math.min(envelope, 1) * 0.55);
        energy += done ? 0 : envelope;
      });
      if (warmth) warmth.style.opacity = String(Math.min(energy / 2.2, 1));
      if (plucks.some(Boolean)) frame = requestAnimationFrame(draw);
    };
    const pluck = (index: number, x: number, amp: number) => {
      plucks[index] = {
        at: performance.now(),
        x: Math.min(Math.max(x, BRIDGE + 24), NUT - 24),
        amp,
      };
      if (!frame) frame = requestAnimationFrame(draw);
    };
    const strum = (amp: number, gap: number) =>
      STRINGS.forEach((_, index) =>
        timers.push(window.setTimeout(() => pluck(index, 190 - index * 6, amp), index * gap)),
      );
    const local = (event: PointerEvent) => {
      const matrix = svg.getScreenCTM();
      return matrix && new DOMPoint(event.clientX, event.clientY).matrixTransform(matrix.inverse());
    };

    const enter = (event: PointerEvent) => {
      const at = local(event);
      if (!at) return;
      last = { y: at.y, t: event.timeStamp };
      // Arriving always answers, even from the side where no lane is crossed.
      const nearest = LANES.reduce((best, lane, i) => (Math.abs(lane - at.y) < Math.abs(LANES[best] - at.y) ? i : best), 0);
      pluck(nearest, at.x, 0.45);
    };
    const move = (event: PointerEvent) => {
      const at = local(event);
      if (!at || !last) return;
      const speed = Math.abs(at.y - last.y) / Math.max(event.timeStamp - last.t, 1);
      for (const index of crossed(last.y, at.y, LANES)) pluck(index, at.x, Math.min(0.4 + speed * 2.5, 1.1));
      last = { y: at.y, t: event.timeStamp };
    };
    const leave = () => {
      last = null;
    };
    const press = () => strum(1, 32);

    svg.addEventListener("pointerenter", enter);
    svg.addEventListener("pointermove", move);
    svg.addEventListener("pointerleave", leave);
    svg.addEventListener("pointerdown", press);
    timers.push(window.setTimeout(() => strum(0.6, 70), 450));
    return () => {
      svg.removeEventListener("pointerenter", enter);
      svg.removeEventListener("pointermove", move);
      svg.removeEventListener("pointerleave", leave);
      svg.removeEventListener("pointerdown", press);
      timers.forEach(clearTimeout);
      cancelAnimationFrame(frame);
    };
  }, []);

  const neck = `M86 ${NECK.top[0]}L${NUT} ${NECK.top[1]}L${NUT} ${NECK.bottom[1]}L86 ${NECK.bottom[0]}Z`;
  const bedStart = FRETS[FRETS.length - 1] - 5;
  const bed = `M${bedStart} ${edge(NECK.top, bedStart) + 1.2}L${NUT} ${NECK.top[1] + 1.2}L${NUT} ${NECK.bottom[1] - 1.2}L${bedStart} ${edge(NECK.bottom, bedStart) - 1.2}Z`;
  const kudam = "M86 37C80 22 61 12.5 42 12.5C21 12.5 7 28 7 45.5C7 63 22 78.5 43 78.5C63 78.5 80 68 86 53";

  return (
    <svg ref={root} className="veena" viewBox="0 0 360 84" aria-hidden="true">
      <defs>
        <radialGradient id={wood} cx="0.38" cy="0.32" r="0.75">
          <stop className="v-wood-hi" offset="0" />
          <stop className="v-wood-lo" offset="1" />
        </radialGradient>
        <radialGradient id={glow} cx="0.45" cy="0.5" r="0.6">
          <stop className="v-glow-hi" offset="0" />
          <stop className="v-glow-lo" offset="1" />
        </radialGradient>
      </defs>

      <path className="v-body" d={kudam} fill={`url(#${wood})`} />
      <path className="v-warmth" d={kudam} fill={`url(#${glow})`} />
      <ellipse className="v-board" cx="44" cy="45.5" rx="28" ry="25" />
      {[29.5, 61.5].map((cy) => (
        <g key={cy} className="v-rosette">
          <circle cx="35" cy={cy} r="2" />
          {ROSETTE.map((angle) => (
            <circle key={angle} className="v-inlay" cx={35 + 5 * Math.cos(angle)} cy={cy + 5 * Math.sin(angle)} r="0.75" />
          ))}
        </g>
      ))}
      <line className="v-tailpiece" x1={TAIL - 1} y1="42.5" x2={TAIL - 1} y2="48.5" />

      <path className="v-neck" d={neck} />
      <path className="v-neck-edge" d={`M86 ${NECK.top[0]}L${NUT} ${NECK.top[1]}M86 ${NECK.bottom[0]}L${NUT} ${NECK.bottom[1]}`} />
      <path className="v-bed" d={bed} />
      {FRETS.map((x) => (
        <line key={x} className="v-fret" x1={x} x2={x} y1={edge(NECK.top, x) + 0.6} y2={edge(NECK.bottom, x) - 0.6} />
      ))}
      <line className="v-nut" x1={NUT} y1={NECK.top[1] + 0.6} x2={NUT} y2={NECK.bottom[1] - 0.6} />

      <path className="v-stem" d="M259.5 50.4V55.5M264.5 50.3V55.5" />
      <circle className="v-body" cx="262" cy="67" r="11.5" fill={`url(#${wood})`} />
      <ellipse className="v-collar" cx="262" cy="55.6" rx="4.6" ry="1.5" />

      <path className="v-peghead" d="M300 39.5L316 40Q319.5 44.5 316 49L300 49.5" />
      {[306, 312].map((x) => (
        <g key={x} className="v-peg">
          <path d={`M${x} 40V34.6M${x} 49V54.4`} />
          <circle cx={x} cy="33.2" r="1.7" />
          <circle cx={x} cy="55.8" r="1.7" />
        </g>
      ))}
      <path
        className="v-yali"
        d="M316.5 40.5C323.5 38.6 328.6 33.4 328.6 26.4C328.6 19.6 323.8 15.8 319.8 17.4C316.4 18.8 316.9 23.2 319.9 23.7C322 24 323.3 22.4 322.7 20.7M317.5 48.8C330.5 48.8 340.5 43.2 344.6 35C345.7 32.8 344.5 30.4 342.2 30.2C337.6 29.8 332.2 30.6 328.6 26.4"
      />
      <circle className="v-eye" cx="335.8" cy="34.6" r="1.05" />

      <line className="v-bridge" x1={BRIDGE} y1="39.6" x2={BRIDGE} y2="50.8" />
      {STRINGS.map((string) => (
        <g key={string.bridge}>
          <path className="v-still" d={`M${TAIL} ${string.tail}L${BRIDGE} ${string.bridge}`} strokeWidth={string.width} />
          <path className="v-ring" d={lens(string, 190, 0)} />
          <path className="v-string" d={bow(string, 190, 0)} strokeWidth={string.width} />
        </g>
      ))}
    </svg>
  );
}
