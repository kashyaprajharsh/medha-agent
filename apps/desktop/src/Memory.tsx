import { useEffect, useState } from "react";
import { Icon } from "./Icon";
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

  async function source(entry: Entry) {
    if (!sessionKey) return;
    const key = `${entry.scope}:${entry.name}`;
    if (sources[key]) return setSources(({ [key]: _, ...rest }) => rest);
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
  const groups = [
    { title: "This project", rows: shown.filter((entry) => entry.scope === "project") },
    { title: "Everywhere", rows: shown.filter((entry) => entry.scope === "user") },
  ];
  return (
    <div className="memory-page">
      <div className="page-heading">
        <div>
          <h2>Memory</h2>
          <p>What Medha keeps between chats. Pinned memories are always included. Changes apply from the next chat.</p>
        </div>
      </div>
      {entries && entries.length > 8 && (
        <input className="filter-input" placeholder="Filter memories" value={query} onChange={(event) => setQuery(event.target.value)} />
      )}
      {error && <p className="surface-error" role="alert">{error}</p>}
      {!entries && !error && <p className="quiet">Reading memory…</p>}
      {entries && !entries.length && <p className="quiet">Nothing remembered yet. Medha saves facts you confirm while you work.</p>}
      {groups.map(
        (group) =>
          group.rows.length > 0 && (
            <section key={group.title} className="memory-group">
              <h3>{group.title}</h3>
              <ul>
                {group.rows.map((entry) => {
                  const key = `${entry.scope}:${entry.name}`;
                  const found = sources[key];
                  return (
                    <li key={key}>
                      <div className="memory-main">
                        <p>{entry.claim}</p>
                        <small>
                          {entry.pinned && <span className="memory-pin"><Icon name="check" />Pinned · </span>}
                          {words(entry.kind)} · {words(entry.confidence)} · from {entry.sessions} {entry.sessions === 1 ? "chat" : "chats"}
                        </small>
                        {found && (
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
                        )}
                      </div>
                      <div className="memory-actions">
                        <button className="link-btn" onClick={() => void source(entry)}>
                          {found ? "Hide source" : "Where it came from"}
                        </button>
                        <button className="link-btn" disabled={busy === key} onClick={() => void change(entry, "memory.pin")}>
                          {entry.pinned ? "Unpin" : "Pin"}
                        </button>
                        {forgetting === key ? (
                          <>
                            <button className="link-btn" onClick={() => setForgetting("")}>Keep</button>
                            <button className="link-btn danger" disabled={busy === key} onClick={() => void change(entry, "memory.forget")}>
                              Forget
                            </button>
                          </>
                        ) : (
                          <button className="link-btn" onClick={() => setForgetting(key)}>Forget</button>
                        )}
                      </div>
                    </li>
                  );
                })}
              </ul>
            </section>
          ),
      )}
    </div>
  );
}
