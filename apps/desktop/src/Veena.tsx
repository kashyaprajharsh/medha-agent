// A Saraswati veena: resonator, fretted neck, upper gourd and yali head. The pluck is pure CSS.
const FRETS = Array.from({ length: 13 }, (_, index) => 86 + index * 14);
const STRINGS = [0, 1, 2, 3];
const NECK_START = 70;
const NECK_END = 282;

export function Veena() {
  return (
    <svg className="veena" viewBox="0 0 320 68" aria-hidden="true">
      <ellipse className="v-bowl" cx="40" cy="36" rx="31" ry="23" />
      <circle className="v-rose" cx="40" cy="36" r="8" />
      <circle className="v-rose-in" cx="40" cy="36" r="2.4" />
      <path className="v-neck" d="M70 27 L282 29 L282 34 L70 40 Z" />
      <line className="v-hang" x1="238" y1="34" x2="238" y2="39" />
      <circle className="v-gourd" cx="238" cy="49" r="10.5" />
      {FRETS.map((x) => {
        const at = (x - NECK_START) / (NECK_END - NECK_START);
        return (
          <line
            key={x}
            className="v-fret"
            x1={x}
            x2={x}
            y1={27 - at * 1.5}
            y2={40 - at * 5.5}
            style={{ animationDelay: `calc(var(--pluck-start) + ${(at * 1.62).toFixed(3)}s)` }}
          />
        );
      })}
      <path
        className="v-yali"
        d="M282 31.5c11 0 19-6 18.5-14.5-.4-6.4-8-8.6-11-4.2-2.4 3.6.6 7.6 4 6"
      />
      <line className="v-bridge" x1="62" y1="27" x2="62" y2="42" />
      {STRINGS.map((index) => (
        <line
          key={index}
          className="v-string"
          x1="62"
          y1={29 + index * 3}
          x2="288"
          y2={29.4 + index * 0.9}
          style={{ animationDelay: `${index * 50}ms` }}
        />
      ))}
      <circle className="v-note" cx="70" cy="33" r="2.6" />
    </svg>
  );
}
