import { useEffect, useState } from "react";

// The TUI's veena track (spin.rs): resonator on slots 0-2, frets on the neck, gourd on 18.
const SLOTS = 19;
const REST = 8;
const FRETS = [4, 6, 8, 10, 12, 14, 16];
const slotX = (slot: number) => 34 + (slot - 3) * (218 / 14);

export function Veena() {
  const [head, setHead] = useState(-1);

  useEffect(() => {
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    let step = 0;
    const timer = setInterval(() => {
      step = (step + 1) % (SLOTS + REST);
      setHead(step < SLOTS ? step : -1);
    }, 48);
    return () => clearInterval(timer);
  }, []);

  const onNeck = head >= 3 && head <= 17;
  return (
    <svg className="veena" viewBox="0 0 272 28" aria-hidden="true">
      <circle
        className={`v-res ${head >= 0 && head <= 2 ? "lit" : ""}`}
        cx="20"
        cy="14"
        r="11"
      />
      <circle className="v-res-in" cx="20" cy="14" r="4" />
      <line className="v-neck" x1="31" y1="14" x2="253" y2="14" />
      {FRETS.map((slot) => {
        const behind = head - slot;
        return (
          <line
            key={slot}
            className={`v-fret ${head >= 0 && behind > 0 && behind < 4 ? `t${behind}` : ""}`}
            x1={slotX(slot)}
            x2={slotX(slot)}
            y1="8"
            y2="20"
          />
        );
      })}
      <circle
        className={`v-gourd ${head === 18 ? "lit" : ""}`}
        cx="259"
        cy="14"
        r="5.5"
      />
      <path
        className={`v-head ${onNeck ? "" : "off"}`}
        d="M0-6 6 0 0 6-6 0Z"
        style={
          onNeck
            ? { transform: `translate(${slotX(head)}px, 14px)` }
            : undefined
        }
      />
    </svg>
  );
}
