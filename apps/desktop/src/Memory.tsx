import { useEffect, useRef, useState } from "react";
import { Find, PageTop, ViewBar } from "./ExtensionParts";
import { Sheet } from "./Sheet";
import { useWorkspaceApi } from "./Workspace";

type Entry = {
  name: string;
  claim: string;
  description: string;
  kind: string;
  scope: "project" | "user";
  trust: string;
  confidence: string;
  pinned: boolean;
  sessions: number;
  updated: number;
};
type Source = { session: string | null; kind?: string; ts?: number; excerpt?: string };

const words = (text: string) => text.replaceAll("_", " ");
const sentence = (text: string) => text.charAt(0).toUpperCase() + text.slice(1);
const keyOf = (entry: Entry) => `${entry.scope}:${entry.name}`;

/** What Medha remembers across chats, where each memory came from, and pin or
 * forget. Changes are recorded in the chat's log and apply from the next chat. */
export function Memory({ sessionKey, ensureOpen }: { sessionKey: string | null; ensureOpen: () => Promise<void> }) {
  const api = useWorkspaceApi();
  const [entries, setEntries] = useState<Entry[]>();
  const [error, setError] = useState("");
  const [query, setQuery] = useState("");
  const [busy, setBusy] = useState("");
  const [forgetting, setForgetting] = useState("");
  const [sources, setSources] = useState<Record<string, Source>>({});
  const [openKey, setOpenKey] = useState("");
  const filter = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!sessionKey) return;
    let active = true;
    void ensureOpen()
      .then(() => api.liveCall(sessionKey, "memory.list"))
      .then((result) => active && setEntries(result.memories as Entry[]))
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [sessionKey]);

  async function change(entry: Entry, method: "memory.pin" | "memory.forget") {
    if (!sessionKey) return;
    const key = `${entry.scope}:${entry.name}`;
    setBusy(key);
    setError("");
    try {
      const result = await api.liveCall(sessionKey, method, { scope: entry.scope, name: entry.name, pinned: !entry.pinned });
      setEntries(result.memories as Entry[]);
      setForgetting("");
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy("");
    }
  }

  // Opening a memory shows where it came from; that is asked for once and kept.
  function show(entry: Entry) {
    setForgetting("");
    setOpenKey(keyOf(entry));
    void source(entry);
  }

  async function source(entry: Entry) {
    if (!sessionKey) return;
    const key = keyOf(entry);
    if (sources[key]) return;
    try {
      const found = (await api.liveCall(sessionKey, "memory.provenance", { scope: entry.scope, name: entry.name })) as Source;
      setSources((all) => ({ ...all, [key]: found }));
    } catch (cause) {
      setError(String(cause));
    }
  }

  if (!sessionKey) return <p className="quiet">Open a chat to see what Medha remembers.</p>;
  const needle = query.toLowerCase();
  const shown = (entries ?? []).filter((entry) => `${entry.name} ${entry.claim} ${entry.description}`.toLowerCase().includes(needle));
  const open = entries?.find((entry) => keyOf(entry) === openKey);
  const found = sources[openKey];
  const groups = [
    { title: "This project", rows: shown.filter((entry) => entry.scope === "project") },
    { title: "Everywhere", rows: shown.filter((entry) => entry.scope === "user") },
  ];
  return (
    <div className="memory-page">
      <PageTop title="Memory">
        What Medha keeps between chats. Pinned memories are always included. Changes apply from the next chat.
      </PageTop>
      {entries && entries.length > 8 && (
        <ViewBar>
          <Find box={filter} label="Find a memory" value={query} onChange={setQuery} />
        </ViewBar>
      )}
      {error && <p className="surface-error" role="alert">{error}</p>}
      {!entries && !error && <p className="quiet">Reading memory…</p>}
      {entries && !entries.length && <p className="quiet">Nothing remembered yet. Medha saves facts you confirm while you work.</p>}
      {groups.map(
        (group) =>
          group.rows.length > 0 && (
            <section key={group.title} className="ix-group">
              <h3>
                {group.title} <span>{group.rows.length}</span>
              </h3>
              <div className="ix">
                {group.rows.map((entry) => {
                  const key = keyOf(entry);
                  return (
                    <div className="ix-row" key={key}>
                      <button className="ix-main whole" onClick={() => show(entry)}>
                        <span className="ix-sum lead">{entry.claim}</span>
                      </button>
                      {entry.pinned && <span className="ix-chip gold">Pinned</span>}
                      <small className="ix-note">
                        from {entry.sessions} {entry.sessions === 1 ? "chat" : "chats"}
                      </small>
                    </div>
                  );
                })}
              </div>
            </section>
          ),
      )}
      {open && (
        <Sheet label={open.claim} onClose={() => setOpenKey("")}>
          <div className="sheet-title">
            <h2 className="claim">{open.claim}</h2>
          </div>
          {open.description && <p className="sheet-more">{open.description}</p>}
          <dl className="sheet-facts">
            <dt>Kept for</dt>
            <dd>{open.scope === "project" ? "This project" : "Every project"}</dd>
            <dt>Kind</dt>
            <dd>{sentence(words(open.kind))}</dd>
            <dt>Confidence</dt>
            <dd>{sentence(words(open.confidence))}</dd>
            <dt>Seen in</dt>
            <dd>
              {open.sessions} {open.sessions === 1 ? "chat" : "chats"}
            </dd>
            <dt>Always used</dt>
            <dd>{open.pinned ? "Yes, it is pinned" : "No, only when it is relevant"}</dd>
          </dl>
          <h3 className="sheet-h">Where it came from</h3>
          {found ? (
            <blockquote className="memory-source">
              {found.session ? (
                <>
                  <span>{found.excerpt || "No text recorded for this step."}</span>
                  {found.ts && <small>{new Date(found.ts * 1000).toLocaleString()}</small>}
                </>
              ) : (
                <span>The chat this came from is no longer in this project’s history.</span>
              )}
            </blockquote>
          ) : (
            <p className="quiet">Looking it up…</p>
          )}
          <div className="sheet-actions">
            <button className="btn-gold" disabled={busy === openKey} onClick={() => void change(open, "memory.pin")}>
              {open.pinned ? "Unpin" : "Pin"}
            </button>
            {forgetting === openKey ? (
              <>
                <button className="btn-line" onClick={() => setForgetting("")}>Keep</button>
                <button className="btn-line danger" disabled={busy === openKey} onClick={() => void change(open, "memory.forget")}>
                  Forget it
                </button>
              </>
            ) : (
              <button className="sheet-quiet" onClick={() => setForgetting(openKey)}>Forget</button>
            )}
          </div>
        </Sheet>
      )}
    </div>
  );
}
