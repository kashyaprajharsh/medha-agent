import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { ArtifactRevision } from "./api";
import { aboutOf, recordedOutput, versionsOf, type Output, type OutputAbout } from "./outputBlocks";
import { OutputsProvider } from "./outputContext";
import { useWorkspaceApi } from "./Workspace";
import { forgetScreens } from "./OutputApp";

type Props = {
  chat: string | null;
  sessionId?: string;
  stamp?: string;
  ended?: boolean;
  running?: boolean;
  /** Outputs the backend sent as they were saved, since the chat's outputs were listed. */
  pushed?: ArtifactRevision[];
  viewing: boolean;
  onShow: () => void;
  /** Opens the viewer for something just made; false when the person is busy elsewhere. */
  onMade: () => boolean;
  onAsk: (text: string) => void;
  /** Sends a correction request now; `about` makes its reply that output's next version. */
  onFix?: (text: string, about?: OutputAbout) => void;
  /** The saved output open beside the chat, which the next message is about unless dismissed. */
  onAbout?: (about: OutputAbout | undefined) => void;
  children: ReactNode;
};

type Entry = { output: Output; order: number };

const toggled = (list: string[], anchor: string, on: boolean) => [
  ...list.filter((other) => other !== anchor),
  ...(on ? [anchor] : []),
];
// A revision is itself; neither labels nor similar content establish identity.
const same = (a: Output, b: Output) => a.anchor === b.anchor || !!a.revisionId && a.revisionId === b.revisionId;

/** Holds one chat's outputs apart from the app shell, so a streaming plate redraws only its readers. */
export function OutputScope({ chat, sessionId, stamp, ended, running, pushed, viewing, onShow, onMade, onAsk, onFix, onAbout, children }: Props) {
  const api = useWorkspaceApi();
  const [open, setOpen] = useState<Output | null>(null);
  const [stopped, setStopped] = useState<string[]>([]);
  const [online, setOnline] = useState<string[]>([]);
  const [entries, setEntries] = useState<{ chat: string | null; list: Entry[] }>({ chat, list: [] });
  const [catalogue, setCatalogue] = useState<{ chat: string | null; entries: Entry[] }>();
  const [notice, setNotice] = useState<string>();
  const [attempt, setAttempt] = useState(0);
  const retry = useCallback(() => setAttempt((value) => value + 1), []);
  useEffect(() => {
    if (!sessionId || !api.artifacts) return;
    let current = true;
    const timer = setTimeout(() => void (async () => {
      const collected: Entry[] = [];
      let cursor = 0; let omitted = 0; let bytes = 0;
      do {
        const page = await api.artifacts(sessionId, cursor);
        if (!current) return;
        omitted += page.omitted;
        bytes += new TextEncoder().encode(JSON.stringify(page.revisions)).byteLength;
        if (bytes > 32 * 1024 * 1024 || collected.length + page.revisions.length > 20_000) throw new Error("This chat's artifact catalogue exceeds the preview limit. Its files and source remain in history.");
        collected.push(...page.revisions.map((record) => ({ output: recordedOutput(record, sessionId), order: record.ts * 1000 + record.source_index })));
        if (page.next_cursor === null) break;
        if (page.next_cursor <= cursor) throw new Error("The artifact catalogue returned an invalid cursor.");
        cursor = page.next_cursor;
      } while (current);
      if (current) {
        setCatalogue({ chat, entries: collected });
        setNotice(omitted ? `${omitted} oversized revisions could not be listed. Their source remains in history.` : undefined);
      }
    })().catch((error) => current && setNotice(`Could not load saved outputs: ${String(error).replace(/^Error:\s*/, "")}`)), 50);
    return () => { current = false; clearTimeout(timer); };
  }, [api, chat, sessionId, stamp, attempt]);
  useEffect(() => { if (chat && ended) forgetScreens(chat); }, [chat, ended]);
  const act = useRef({ onShow, onMade, onAsk, onFix, onAbout });
  act.current = { onShow, onMade, onAsk, onFix, onAbout };
  const seen = useRef(open);
  seen.current = open;
  const from = useRef<DOMRect | null>(null);
  // Whether the person is on the newest version, so a revision may take its place.
  const newest = useRef(true);
  // The person picked this version themselves; it is never swapped under them.
  const chosen = useRef(false);
  const previousChat = useRef(chat);
  useEffect(() => {
    if (previousChat.current === chat) return;
    previousChat.current = chat;
    setOpen(null);
    setStopped([]);
    setOnline([]);
    setNotice(undefined);
    chosen.current = false;
    newest.current = true;
  }, [chat]);

  const saved = useMemo(() => {
    const listed = catalogue?.chat === chat ? catalogue.entries : [];
    if (!pushed?.length || !sessionId) return listed;
    const known = new Set(listed.map((entry) => entry.output.revisionId));
    const added = pushed
      .filter((record) => !known.has(record.revision_id))
      .map((record) => ({ output: recordedOutput(record, sessionId), order: record.ts * 1000 + record.source_index }));
    return added.length ? [...listed, ...added] : listed;
  }, [catalogue, chat, pushed, sessionId]);
  const index = useMemo(() => {
    const anchors = new Map<string, Entry>();
    const sources = new Map<string, Entry>();
    for (const entry of saved) {
      const output = entry.output;
      anchors.set(output.anchor, entry);
      if (output.sourceEvent) sources.set(`${output.sourceEvent}\0${output.sourceIndex}\0${output.kind === "slides" ? "page" : output.kind}`, entry);
    }
    return { anchors, sources };
  }, [saved]);
  const resolve = useCallback((output: Output) => {
    const matching = (index.anchors.get(output.anchor) ??
      (output.sourceEvent ? index.sources.get(`${output.sourceEvent}\0${output.sourceIndex}\0${output.kind === "slides" ? "page" : output.kind}`) : undefined))?.output;
    return matching ? { ...matching, source: matching.source || output.source, kind: output.kind, name: output.name } : output;
  }, [index]);
  const all = useMemo(() => {
    const byId = new Map<string, Entry>();
    for (const entry of [...saved, ...(entries.chat === chat ? entries.list : [])]) {
      const output = resolve(entry.output);
      // Before publication a streaming plate has only a temporary anchor.
      // It stays visible in the chat; the durable catalogue owns All outputs.
      if (typeof api.artifacts === "function" && sessionId && !output.revisionId) continue;
      byId.set(output.anchor, { ...entry, output, order: index.anchors.get(output.anchor)?.order ?? entry.order });
    }
    return [...byId.values()].sort((a, b) => a.order - b.order).map((entry) => entry.output);
  }, [entries, chat, saved, resolve, index, api, sessionId]);
  useEffect(() => {
    if (!open) return;
    const bound = resolve(open);
    const chain = versionsOf(all, bound);
    const last = chain.at(-1)!;
    const here = all.find((other) => same(other, bound));
    // A saved reply takes over the anchor of the live one it was written as.
    if (chosen.current) chosen.current = false;
    else if (here && here !== open) return setOpen(here);
    else if (newest.current && last.anchor !== open.anchor) return setOpen(last);
    newest.current = last.anchor === open.anchor;
  }, [all, open, resolve]);

  const enter = useCallback((output: Output, order: number) => {
    setEntries((held) => ({ chat, list: [...(held.chat === chat ? held.list : []).filter((entry) => entry.output.anchor !== output.anchor), { output, order }] }));
    return () => setEntries((held) => held.chat !== chat ? held : { chat, list: held.list.filter((entry) => entry.output.anchor !== output.anchor) });
  }, [chat]);
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
    if ((seen.current && (!newest.current || seen.current.artifactId && output.artifactId && seen.current.artifactId !== output.artifactId)) || !act.current.onMade()) return;
    from.current = null;
    setOpen({ ...output });
  }, []);
  const ask = useCallback((text: string) => act.current.onAsk(text), []);
  const fix = useCallback((output: Output, text: string) => {
    if (act.current.onFix) act.current.onFix(text, aboutOf(resolve(output)));
    else act.current.onAsk(text);
  }, [resolve]);
  // What is open beside the chat is what the next message is about. Reported
  // by its two IDs, so a redraw of the same version does not report again.
  const reference = aboutOf(viewing && open ? resolve(open) : null);
  const referred = reference && `${reference.artifact_id}\0${reference.revision_id}\0${reference.label}`;
  useEffect(() => {
    act.current.onAbout?.(reference);
  }, [referred]);
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
    () => ({ open, all, viewing, stopped, online, show, follow, enter, made, ask, fix, stop, allow, browse, from, resolve, notice, retry, running }),
    [open, all, viewing, stopped, online, show, follow, enter, made, ask, fix, stop, allow, browse, resolve, notice, retry, running],
  );
  return <OutputsProvider value={value}>{children}</OutputsProvider>;
}
