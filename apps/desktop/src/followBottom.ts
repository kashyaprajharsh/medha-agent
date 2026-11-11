import { useCallback, useEffect, useRef, useState } from "react";

/** How close to the end counts as at the end, in pixels. */
const NEAR = 32;
/** How long after a wheel, touch, key or scrollbar press a scroll still counts as the person's own. */
const INTENT_MS = 700;
const SCROLL_KEYS = new Set(["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End", " "]);

/**
 * Keeps a chat's scroll box at its latest message while content grows, and lets the person leave it.
 * Only the person's own input (wheel, touch, keys, the scrollbar) moves them away from the end; content
 * changing size never does. Each chat key remembers where it was left, so reopening it returns there.
 */
export function useFollowBottom(key: string | null) {
  const [node, setNode] = useState<HTMLElement | null>(null);
  const [away, setAwayState] = useState(false);
  const awayRef = useRef(false);
  const intent = useRef(0);
  // Per chat: how far from the end it was left, or nothing when it was left at the end.
  const left = useRef(new Map<string, number>());
  const restore = useRef<number | null>(null);

  const setAway = useCallback((value: boolean) => {
    awayRef.current = value;
    setAwayState(value);
  }, []);

  const end = (box: HTMLElement) => Math.max(0, box.scrollHeight - box.clientHeight);

  const toBottom = useCallback(
    (smooth = false) => {
      restore.current = null;
      setAway(false);
      if (!node) return;
      const reduce = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
      if (smooth && !reduce && typeof node.scrollTo === "function")
        node.scrollTo({ top: end(node), behavior: "smooth" });
      else node.scrollTop = end(node);
    },
    [node, setAway],
  );

  // Switching chats: remember where the old one was left and pick up the new one where it was left.
  const previous = useRef<string | null>(key);
  useEffect(() => {
    const box = node;
    const old = previous.current;
    if (box && old && old !== key) {
      if (awayRef.current) left.current.set(old, end(box) - box.scrollTop);
      else left.current.delete(old);
    }
    previous.current = key;
    const distance = key ? left.current.get(key) : undefined;
    restore.current = distance ?? null;
    setAway(distance !== undefined);
    intent.current = 0;
  }, [key, node, setAway]);

  useEffect(() => {
    const box = node;
    if (!box) return;
    const place = () => {
      if (restore.current !== null) box.scrollTop = Math.max(0, end(box) - restore.current);
      else if (!awayRef.current) box.scrollTop = end(box);
    };
    const mark = () => (intent.current = Date.now());
    const onKey = (event: KeyboardEvent) => SCROLL_KEYS.has(event.key) && mark();
    const onScroll = () => {
      const atEnd = end(box) - box.scrollTop < NEAR;
      if (atEnd) {
        restore.current = null;
        if (awayRef.current) setAway(false);
      } else if (Date.now() - intent.current < INTENT_MS) {
        // The person moved; from here their own position wins over any remembered one.
        restore.current = null;
        if (!awayRef.current) setAway(true);
      }
    };
    // Late content (history pages, highlighted code, images) grows the box; follow or hold the place.
    const grown = new ResizeObserver(place);
    grown.observe(box);
    for (const child of Array.from(box.children)) grown.observe(child);
    const added = new MutationObserver(place);
    added.observe(box, { childList: true, subtree: true });
    box.addEventListener("wheel", mark, { passive: true });
    box.addEventListener("touchmove", mark, { passive: true });
    box.addEventListener("pointerdown", mark);
    box.addEventListener("keydown", onKey);
    box.addEventListener("scroll", onScroll, { passive: true });
    place();
    return () => {
      grown.disconnect();
      added.disconnect();
      box.removeEventListener("wheel", mark);
      box.removeEventListener("touchmove", mark);
      box.removeEventListener("pointerdown", mark);
      box.removeEventListener("keydown", onKey);
      box.removeEventListener("scroll", onScroll);
    };
  }, [node, setAway]);

  return { ref: setNode, away, toBottom };
}
