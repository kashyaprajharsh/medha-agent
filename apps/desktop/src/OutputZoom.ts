import { type RefObject, useEffect, useLayoutEffect, useRef } from "react";

export const ZOOMS = [0.5, 0.75, 1, 1.5, 2, 3, 4, 6, 8];
const LEAST = ZOOMS[0];
const MOST = ZOOMS[ZOOMS.length - 1];

/** The next button step from a zoom that pinching may have left between steps. */
export function stepZoom(zoom: number | undefined, by: number): number {
  const now = zoom ?? 1;
  const next = by > 0 ? ZOOMS.find((step) => step > now + 1e-3) : [...ZOOMS].reverse().find((step) => step < now - 1e-3);
  return next ?? (by > 0 ? MOST : LEAST);
}

function typing(target: EventTarget | null) {
  return target instanceof HTMLElement && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName));
}

/**
 * Pinch or ⌘/Ctrl-scroll zooms at the pointer, a drag pans once zoomed, a double
 * click zooms in there (or back to Fit when close up), and + - 0 work from the keyboard.
 */
export function useZoomGestures(
  canvas: RefObject<HTMLDivElement | null>,
  enabled: boolean,
  zoom: number | undefined,
  setZoom: (zoom: number | undefined) => void,
  rebind: unknown,
) {
  const current = useRef(zoom);
  current.current = zoom;
  // The point under the pointer, kept in place after the zoom lands.
  const anchor = useRef<{ x: number; y: number; ratio: number }>(undefined);

  const zoomAt = (next: number | undefined, x?: number, y?: number) => {
    const room = canvas.current;
    const before = current.current ?? 1;
    const after = next === undefined ? undefined : Math.min(MOST, Math.max(LEAST, next));
    if (room && after !== undefined && x !== undefined && y !== undefined) {
      const box = room.getBoundingClientRect();
      anchor.current = { x: x - box.left, y: y - box.top, ratio: after / before };
    }
    setZoom(after);
  };

  useLayoutEffect(() => {
    const room = canvas.current;
    const at = anchor.current;
    anchor.current = undefined;
    if (!room || !at || zoom === undefined) return;
    room.scrollLeft = (room.scrollLeft + at.x) * at.ratio - at.x;
    room.scrollTop = (room.scrollTop + at.y) * at.ratio - at.y;
  }, [canvas, zoom]);

  useEffect(() => {
    const room = canvas.current;
    if (!room || !enabled) return;
    const wheel = (event: WheelEvent) => {
      if (!event.ctrlKey && !event.metaKey) return;
      event.preventDefault();
      zoomAt((current.current ?? 1) * Math.exp(-event.deltaY * 0.01), event.clientX, event.clientY);
    };
    let drag: { x: number; y: number; left: number; top: number; id: number } | undefined;
    const down = (event: PointerEvent) => {
      if (event.button !== 0 || current.current === undefined) return;
      drag = { x: event.clientX, y: event.clientY, left: room.scrollLeft, top: room.scrollTop, id: event.pointerId };
      room.setPointerCapture?.(event.pointerId);
      room.classList.add("panning");
    };
    const move = (event: PointerEvent) => {
      if (!drag || event.pointerId !== drag.id) return;
      room.scrollLeft = drag.left - (event.clientX - drag.x);
      room.scrollTop = drag.top - (event.clientY - drag.y);
    };
    const up = (event: PointerEvent) => {
      if (!drag || event.pointerId !== drag.id) return;
      drag = undefined;
      room.classList.remove("panning");
    };
    const double = (event: MouseEvent) => {
      const now = current.current ?? 1;
      if (now >= 4) zoomAt(undefined);
      else zoomAt(now * 2, event.clientX, event.clientY);
    };
    const key = (event: KeyboardEvent) => {
      if (typing(event.target) || event.altKey) return;
      if (event.key === "+" || event.key === "=") setZoom(stepZoom(current.current, 1));
      else if (event.key === "-" || event.key === "_") setZoom(stepZoom(current.current, -1));
      else if (event.key === "0") setZoom(undefined);
      else return;
      event.preventDefault();
    };
    room.addEventListener("wheel", wheel, { passive: false });
    room.addEventListener("pointerdown", down);
    room.addEventListener("pointermove", move);
    room.addEventListener("pointerup", up);
    room.addEventListener("pointercancel", up);
    room.addEventListener("dblclick", double);
    window.addEventListener("keydown", key);
    return () => {
      room.removeEventListener("wheel", wheel);
      room.removeEventListener("pointerdown", down);
      room.removeEventListener("pointermove", move);
      room.removeEventListener("pointerup", up);
      room.removeEventListener("pointercancel", up);
      room.removeEventListener("dblclick", double);
      window.removeEventListener("keydown", key);
      room.classList.remove("panning");
    };
    // zoomAt reads refs only; a remounted canvas (large view) passes a new `rebind`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [canvas, enabled, setZoom, rebind]);
}
