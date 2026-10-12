import { useEffect, useState } from "react";
import { PageTop } from "./ExtensionParts";
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
  /** More keys for the same model server, in the order they are tried after the first. */
  extra?: { slot: number; present: boolean }[];
};
type Listing = { models: Key[]; search: Key[]; mcp: Key[]; store: string; keychain: boolean; forced: boolean };

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

  async function run(method: string, key: Key, secret?: string, which: { add?: boolean; slot?: number } = {}) {
    setBusy(true);
    setError("");
    try {
      await api.settings(method, { group: key.group, id: key.id, key: secret, ...which });
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

  /** Keep keys in the keychain or the private file; the saved ones move with the choice. */
  async function keep(keychain: boolean) {
    if (!listing || listing.keychain === keychain) return;
    setBusy(true);
    setError("");
    try {
      await api.settings("settings.keys.store", { keychain });
      setRefresh((count) => count + 1);
      window.dispatchEvent(new CustomEvent("medha-preferences"));
    } catch (cause) {
      setError(String(cause).replace(/^Error:\s*/, ""));
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
    const extra = key.extra ?? [];
    const adding = editing === `${slot}:add`;
    return [
      <li key={slot} className="ix-row key">
        <span className="ix-main" title={key.label}>
          <b>{key.label}</b>
          <code className="ix-sum mono">{key.detail?.replace(/^https:\/\//, "")}</code>
        </span>
        {key.in_use && <span className="ix-chip gold">In use</span>}
        {extra.length > 0 && <span className="ix-chip">Key 1 of {extra.length + 1}</span>}
        <small className={`ix-note ${key.present || key.signed_in ? "ok" : ""}`}>{status}</small>
        {editing === slot || adding ? (
          <form
            className="key-edit"
            onSubmit={(event) => {
              event.preventDefault();
              void run("settings.keys.set", key, value, adding ? { add: true } : {});
            }}
          >
            <input type="password" autoComplete="new-password" autoFocus aria-label={adding ? `Another key for ${key.label}` : `Key for ${key.label}`} placeholder={adding ? "Another key" : undefined} value={value} onChange={(event) => setValue(event.target.value)} />
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
            {key.present && key.group === "model" && (
              <button className="link-btn" onClick={() => { setEditing(`${slot}:add`); setRemoving(""); }}>
                Add another
              </button>
            )}
            {key.present && <button className="link-btn" onClick={() => setRemoving(slot)}>Remove</button>}
          </span>
        )}
      </li>,
      // The keys tried after the first, in order, when one is refused or limited.
      ...extra.map((more, at) => {
        const own = `${slot}:${more.slot}`;
        return (
          <li key={own} className="ix-row key">
            <span className="ix-main">
              <b>Key {at + 2}</b>
              <code className="ix-sum mono">{key.detail?.replace(/^https:\/\//, "")}</code>
            </span>
            <small className={`ix-note ${more.present ? "ok" : ""}`}>{more.present ? "Set" : "Not set"}</small>
            <span className="key-actions">
              {removing === own ? (
                <>
                  <button className="link-btn" onClick={() => setRemoving("")}>Keep</button>
                  <button className="link-btn danger" disabled={busy} onClick={() => void run("settings.keys.remove", key, undefined, { slot: more.slot })}>
                    Remove
                  </button>
                </>
              ) : (
                <button className="link-btn" onClick={() => { setRemoving(own); setEditing(""); }}>Remove</button>
              )}
            </span>
          </li>
        );
      }),
    ];
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
      <PageTop title="Keys">
        Every key Medha uses. Values are never shown. A model's server can have more than one: when a key is
        refused or limited, the next one is used.
      </PageTop>
      {error && <p className="surface-error" role="alert">{error}</p>}
      {listing && (
        <div className="key-store">
          <span>
            <b>Where keys are kept</b>
            <small>
              {listing.forced
                ? `In ${listing.store}. MEDHA_CRED_STORE is set on this computer, so it decides.`
                : listing.keychain
                  ? `In ${listing.store}. It may ask you to allow Medha again after an update.`
                  : `In ${listing.store}.`}
            </small>
          </span>
          <div className="segmented" role="group" aria-label="Where keys are kept">
            <button aria-pressed={!listing.keychain} disabled={busy || listing.forced} onClick={() => void keep(false)}>
              Private file
            </button>
            <button aria-pressed={listing.keychain} disabled={busy || listing.forced} onClick={() => void keep(true)}>
              System keychain
            </button>
          </div>
        </div>
      )}
      {!listing && !error && <p className="quiet">Reading keys…</p>}
      {groups.map(
        ([title, keys]) =>
          keys.length > 0 && (
            <section className="ix-group" key={title}>
              <h3>
                {title} <span>{keys.length}</span>
              </h3>
              <ul className="ix key-list">{keys.flatMap(row)}</ul>
            </section>
          ),
      )}
      {listing && !listing.models.length && !listing.mcp.length && (
        <p className="quiet">No saved model or server needs a key. Local models run without one.</p>
      )}
    </div>
  );
}
