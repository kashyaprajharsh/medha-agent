import { useEffect, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { Icon } from "./Icon";

/**
 * The one thing a row opens: its full detail, from the right, over the page.
 * It leaves the way it came, so `onClose` is told only once it is gone.
 */
export function Sheet({ label, onClose, children }: { label: string; onClose: () => void; children: ReactNode }) {
  const [leaving, setLeaving] = useState(false);
  const close = () => (matchMedia("(prefers-reduced-motion: reduce)").matches ? onClose() : setLeaving(true));
  useEffect(() => {
    if (!leaving) return;
    // The animation end is the cue; this is for a window that never paints it.
    const done = setTimeout(onClose, 600);
    return () => clearTimeout(done);
  }, [leaving, onClose]);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => event.key === "Escape" && close();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });
  return createPortal(
    <>
      <div className={`cx-scrim${leaving ? " leaving" : ""}`} onClick={close} />
      <aside
        className={`cx-sheet${leaving ? " leaving" : ""}`}
        role="dialog"
        aria-modal="true"
        aria-label={label}
        onAnimationEnd={(event) => {
          if (leaving && event.target === event.currentTarget) onClose();
        }}
      >
        <button className="cx-close" aria-label="Close" autoFocus onClick={close}>
          <Icon name="x" />
        </button>
        {children}
      </aside>
    </>,
    document.body,
  );
}
