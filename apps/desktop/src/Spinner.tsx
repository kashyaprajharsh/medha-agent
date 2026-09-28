import { useEffect, useState } from "react";

const STARS = ["✧", "✦", "✶", "✸", "✹", "✺", "✹", "✷"];

/** The TUI's star spinner (spin.rs), held still when motion is reduced. */
export function Spinner() {
  const [frame, setFrame] = useState(0);
  useEffect(() => {
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const timer = setInterval(
      () => setFrame((value) => (value + 1) % STARS.length),
      110,
    );
    return () => clearInterval(timer);
  }, []);
  return (
    <span className="spinner" aria-hidden="true">
      {STARS[frame]}
    </span>
  );
}
