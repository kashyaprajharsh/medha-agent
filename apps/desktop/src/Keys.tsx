import { useEffect, useState } from "react";
import { useWorkspaceApi } from "./Workspace";

type Key = {
  group: "model" | "search" | "mcp";
  id: string;
  label: string;
  detail?: string;
  present: boolean;
  in_use?: boolean;
  oauth?: boolean;
  signed_in?: boolean;
};
type Listing = { models: Key[]; search: Key[]; mcp: Key[]; store: string };

/** Every key Medha uses, on one page. Values are never shown or sent back. */
export function Keys() {
  const api = useWorkspaceApi();
  const [listing, setListing] = useState<Listing>();
  const [editing, setEditing] = useState("");
  const [removing, setRemoving] = useState("");
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [refresh, setRefresh] = useState(0);

  useEffect(() => {
    let active = true;
    void api
      .settings("settings.keys")
      .then((result) => active && setListing(result as unknown as Listing))
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [refresh]);

  async function run(method: string, key: Key, secret?: string) {
    setBusy(true);
    setError("");
    try {
      await api.settings(method, { group: key.group, id: key.id, key: secret });
      setEditing("");
      setRemoving("");
      setValue("");
      setRefresh((count) => count + 1);
      window.dispatchEvent(new CustomEvent("medha-preferences"));
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(false);
    }
  }

  const row = (key: Key) => {
    const slot = `${key.group}:${key.id}`;
    const status = key.present
      ? "Set"
      : key.oauth
        ? key.signed_in
          ? "Signed in with OAuth"
          : "Signs in with OAuth"
        : "Not set";
    return (
      <li key={slot}>
        <div className="key-main">
          <b>{key.label}</b>
          {key.detail && <code>{key.detail}</code>}
        </div>
        <span className={`key-status ${key.present || key.signed_in ? "set" : ""}`}>
          {status}
          {key.in_use && " · in use"}
        </span>
        {editing === slot ? (
          <form
            className="key-edit"
            onSubmit={(event) => {
              event.preventDefault();
              void run("settings.keys.set", key, value);
            }}
          >
            <input type="password" autoComplete="new-password" autoFocus aria-label={`Key for ${key.label}`} value={value} onChange={(event) => setValue(event.target.value)} />
            <button className="btn-gold" disabled={busy || !value.trim()}>Save</button>
            <button type="button" className="link-btn" onClick={() => { setEditing(""); setValue(""); }}>Cancel</button>
          </form>
        ) : removing === slot ? (
          <span className="key-actions">
            <button className="link-btn" onClick={() => setRemoving("")}>Keep</button>
            <button className="link-btn danger" disabled={busy} onClick={() => void run("settings.keys.remove", key)}>Remove</button>
          </span>
        ) : (
          <span className="key-actions">
            <button className="link-btn" onClick={() => { setEditing(slot); setRemoving(""); }}>
              {key.present ? "Replace" : "Set key"}
            </button>
            {key.present && <button className="link-btn" onClick={() => setRemoving(slot)}>Remove</button>}
          </span>
        )}
      </li>
    );
  };

  const groups: [string, Key[]][] = listing
    ? [
        ["Models", listing.models],
        ["Web search", listing.search],
        ["MCP servers", listing.mcp],
      ]
    : [];
  return (
    <div className="keys-page">
      <div className="page-heading">
        <div>
          <h2>Keys</h2>
          <p>Every key Medha uses. Values are never shown{listing ? `; new keys are kept in ${listing.store}` : ""}.</p>
        </div>
      </div>
      {error && <p className="surface-error" role="alert">{error}</p>}
      {!listing && !error && <p className="quiet">Reading keys…</p>}
      {groups.map(
        ([title, keys]) =>
          keys.length > 0 && (
            <section className="key-group" key={title}>
              <h3>{title}</h3>
              <ul>{keys.map(row)}</ul>
            </section>
          ),
      )}
      {listing && !listing.models.length && !listing.mcp.length && (
        <p className="quiet">No saved model or server needs a key. Local models run without one.</p>
      )}
    </div>
  );
}
