import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { versionsOf, type Output } from "./outputBlocks";
import { OutputsProvider } from "./outputContext";

type Props = {
  chat: string | null;
  viewing: boolean;
  onShow: () => void;
  /** Opens the viewer for something just made; false when the person is busy elsewhere. */
  onMade: () => boolean;
  onAsk: (text: string) => void;
  children: ReactNode;
};

type Entry = { output: Output; order: number };

const toggled = (list: string[], anchor: string, on: boolean) => [
  ...list.filter((other) => other !== anchor),
  ...(on ? [anchor] : []),
];
// A screen is only ever itself; anything else is the same thing wherever its content matches.
const same = (a: Output, b: Output) =>
  !a.screen && !b.screen && a.kind === b.kind && a.source === b.source && a.path === b.path;

/** Holds one chat's outputs apart from the app shell, so a streaming plate redraws only its readers. */
export function OutputScope({ chat, viewing, onShow, onMade, onAsk, children }: Props) {
  const [open, setOpen] = useState<Output | null>(null);
  const [stopped, setStopped] = useState<string[]>([]);
  const [online, setOnline] = useState<string[]>([]);
  const [entries, setEntries] = useState<Entry[]>([]);
  const act = useRef({ onShow, onMade, onAsk });
  act.current = { onShow, onMade, onAsk };
  const seen = useRef(open);
  seen.current = open;
  const from = useRef<DOMRect | null>(null);
  // Whether the person is on the newest version, so a revision may take its place.
  const newest = useRef(true);
  // The person picked this version themselves; it is never swapped under them.
  const chosen = useRef(false);
  useEffect(() => {
    setOpen(null);
    setStopped([]);
    setOnline([]);
  }, [chat]);

  const all = useMemo(() => entries.map((entry) => entry.output), [entries]);
  useEffect(() => {
    if (!open) return;
    const chain = versionsOf(all, open);
    const last = chain.at(-1)!;
    const here = all.find((other) => other.anchor === open.anchor) ?? all.find((other) => same(other, open));
    // A saved reply takes over the anchor of the live one it was written as.
    if (chosen.current) chosen.current = false;
    else if (here && here.anchor !== open.anchor) return setOpen(here);
    else if (newest.current && last.anchor !== open.anchor) return setOpen(last);
    newest.current = last.anchor === open.anchor;
  }, [all, open]);

  const enter = useCallback((output: Output, order: number) => {
    setEntries((list) =>
      [...list.filter((entry) => entry.output.anchor !== output.anchor), { output, order }].sort(
        (a, b) => a.order - b.order,
      ),
    );
    return () => setEntries((list) => list.filter((entry) => entry.output.anchor !== output.anchor));
  }, []);
  const show = useCallback((output: Output, rect?: DOMRect) => {
    from.current = rect ?? null;
    chosen.current = true;
    // A fresh object, so choosing what is already open still settles the choice.
    setOpen({ ...output });
    act.current.onShow();
  }, []);
  const follow = useCallback((output: Output) => {
    chosen.current = true;
    setOpen({ ...output });
  }, []);
  // What Medha has just finished opens beside the chat by itself, unless the
  // person is looking at something else there or at an older version.
  const made = useCallback((output: Output) => {
    if ((seen.current && !newest.current) || !act.current.onMade()) return;
    from.current = null;
    setOpen({ ...output });
  }, []);
  const ask = useCallback((text: string) => act.current.onAsk(text), []);
  const stop = useCallback(
    (output: Output, halt: boolean) => setStopped((list) => toggled(list, output.anchor, halt)),
    [],
  );
  // The web is granted to one page by the person's own click; a revision asks again.
  const allow = useCallback(
    (output: Output, on: boolean) => setOnline((list) => toggled(list, output.anchor, on)),
    [],
  );
  const browse = useCallback(() => setOpen(null), []);

  const value = useMemo(
    () => ({ open, all, viewing, stopped, online, show, follow, enter, made, ask, stop, allow, browse, from }),
    [open, all, viewing, stopped, online, show, follow, enter, made, ask, stop, allow, browse],
  );
  return <OutputsProvider value={value}>{children}</OutputsProvider>;
}
