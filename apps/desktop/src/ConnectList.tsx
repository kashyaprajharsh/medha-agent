import { useEffect, useMemo, useRef, useState } from "react";
import { ChatGptMark, ContinueWithChatGpt } from "./ChatGpt";
import { Icon } from "./Icon";
import { ProviderLogo } from "./ProviderLogo";
import { Spinner } from "./Spinner";
import { useWorkspaceApi } from "./Workspace";
import { hostOf, isLocal, plainName, profileFor, type Found, type Protocol, type Provider, type UnavailableProvider } from "./modelSetup";

export type Listed = Provider & { protocol: string };
export type Chosen =
  | { kind: "provider"; provider: Listed; key?: string }
  | { kind: "server"; server: Listed }
  | { kind: "custom" }
  | { kind: "chatgpt" };
type Recognised = { match: "one" | "several" | "unknown"; providers: { id: string; name: string; available: boolean; reason: string | null }[]; warning?: string };
type Filter = "all" | "plans" | "keys" | "local" | "custom";

/** A pasted key, not a name: one long word with letters and digits. */
const looksLikeKey = (text: string) => /^\S{16,}$/.test(text) && /\d/.test(text) && /[a-z]/i.test(text);

/** Where a model can come from: a plan, a key, this computer, or a server of one's own (mocks C, C1, C3). */
export function ConnectList({ protocols, servers, savedKeys, unavailable, onChoose, onClose, onRefresh, refreshing, refreshError, closeLabel = "Close" }: {
  protocols: Protocol[];
  closeLabel?: string;
  /** The person's own servers from saved models, offered again under Custom. */
  servers: Listed[];
  onClose: () => void;
  /** Addresses that already hold a saved key. */
  savedKeys: string[];
  unavailable: UnavailableProvider[];
  onChoose: (chosen: Chosen) => void;
  onRefresh: () => void;
  refreshing: boolean;
  refreshError: string;
}) {
  const api = useWorkspaceApi();
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<Filter>("all");
  const [recognised, setRecognised] = useState<Recognised>();
  const [running, setRunning] = useState<Record<string, Found[]>>({});
  const asked = useRef(0);
  const search = useRef<HTMLInputElement>(null);
  // After mount, so typing reaches the search even when the sheet around it focused its Close button.
  useEffect(() => search.current?.focus(), []);
  const listed: Listed[] = useMemo(
    () => protocols.filter((item) => item.available).flatMap((item) => item.providers.map((provider) => ({ ...provider, protocol: item.value }))),
    [protocols],
  );
  const local = listed.filter((provider) => isLocal(provider.url));
  const keyed = listed.filter((provider) => !isLocal(provider.url));

  // Local servers are looked for unasked, so one that is running needs no setup.
  useEffect(() => {
    let live = true;
    for (const provider of local.filter((entry) => entry.auth === "none"))
      void api.settings("settings.model.discover", { profile: profileFor({ protocol: provider.protocol, url: provider.url, auth: "none" }), key: "" })
        .then((reply) => { const models = reply.models as Found[]; if (live && models.length) setRunning((all) => ({ ...all, [provider.url]: models })); })
        .catch(() => {});
    return () => { live = false; };
  }, [api, protocols]); // eslint-disable-line react-hooks/exhaustive-deps

  const pasted = looksLikeKey(query.trim()) ? query.trim() : "";
  useEffect(() => {
    const mine = ++asked.current;
    setRecognised(undefined);
    if (!pasted) return;
    void api.settings("settings.model.recognise_key", { key: pasted })
      .then((reply) => { if (mine === asked.current) setRecognised(reply as Recognised); })
      .catch(() => {});
  }, [api, pasted]);

  const byId = (id: string) => listed.find((provider) => provider.id === id);
  const words = query.trim().toLowerCase();
  const shows = (provider: Listed) => !words || pasted !== "" || provider.name.toLowerCase().includes(words) || (provider.id ?? "").includes(words);
  const savedFirst = [...keyed].sort((a, b) => Number(savedKeys.includes(b.url)) - Number(savedKeys.includes(a.url)));
  const want = (group: Filter) => filter === "all" || filter === group;

  const row = (provider: Listed, state: React.ReactNode, key?: string) => (
    <button type="button" key={`${provider.protocol}:${provider.id ?? provider.url}`} className="pv-row" onClick={() => onChoose({ kind: "provider", provider, key })}>
      <ProviderLogo id={provider.id} name={provider.name} />
      <span className="pv-text"><b>{plainName(provider.name)}</b><small>{isLocal(provider.url) ? hostOf(provider.url) : provider.models?.length ? `${provider.models.length} ${provider.models.length === 1 ? "model" : "models"}` : hostOf(provider.url)}</small></span>
      {state}
    </button>
  );
  const keyState = (provider: Listed) =>
    savedKeys.includes(provider.url) ? <span className="pv-state ok"><span className="cg-dot" />Key saved</span> : <span className="pv-state">Add key</span>;

  return (
    <div className="pv-list">
      <div className="pv-search">
        <Icon name="search" />
        <input ref={search} aria-label="Search providers, or paste a key" placeholder={`Search ${listed.length} providers, or paste a key`} value={query} spellCheck={false} autoComplete="off"
          className={pasted ? "filled" : ""} onChange={(event) => setQuery(event.target.value)} />
        <button type="button" className="icon-btn" aria-label="Refresh provider catalogue" title="Refresh provider catalogue" aria-busy={refreshing} disabled={refreshing} onClick={onRefresh}>
          {refreshing ? <Spinner /> : <Icon name="refresh" />}
        </button>
      </div>
      {refreshError && <p className="pv-error" role="alert">{refreshError}</p>}

      {pasted && recognised && <Recognition recognised={recognised} byId={byId} pasted={pasted} onChoose={onChoose} onBrowse={() => setQuery("")} />}

      {!pasted && <div className="pv-filters" role="tablist" aria-label="Show">
        {([["all", "All"], ["plans", "Plans"], ["keys", "API keys"], ["local", "This computer"], ["custom", "Custom"]] as [Filter, string][]).map(([id, label]) =>
          <button type="button" role="tab" key={id} aria-selected={filter === id} onClick={() => setFilter(id)}>{label}</button>)}
      </div>}

      {(!pasted || recognised?.match === "unknown") && <div className="pv-rows">
        {want("plans") && !words && <>
          <p className="pv-group">Plans</p>
          <div className="pv-row">
            <span className="pv-tile"><ChatGptMark size={20} /></span>
            <span className="pv-text"><b>ChatGPT</b><small>Plus, Pro, or Business, with Codex. No key needed.</small></span>
            <ContinueWithChatGpt onClick={() => onChoose({ kind: "chatgpt" })} />
          </div>
        </>}
        {want("keys") && savedFirst.some(shows) && <>
          <p className="pv-group">API keys</p>
          {savedFirst.filter(shows).map((provider) => row(provider, keyState(provider), pasted || undefined))}
        </>}
        {want("local") && local.some(shows) && <>
          <p className="pv-group">This computer</p>
          {local.filter(shows).map((provider) => row(provider, running[provider.url]
            ? <span className="pv-state ok"><span className="cg-dot" />Running, {running[provider.url].length} {running[provider.url].length === 1 ? "model" : "models"}</span>
            : <span className="pv-state">Not running</span>))}
        </>}
        {want("custom") && (servers.some(shows) || !words) && <>
          <p className="pv-group">Custom</p>
          {servers.filter(shows).map((server) => (
            <button type="button" key={`${server.protocol}:${server.url}`} className="pv-row" onClick={() => onChoose({ kind: "server", server })}>
              <ProviderLogo id={server.id} name={server.name} />
              <span className="pv-text"><b>{server.name}</b><small>Your server, {server.url.replace(/^https?:\/\//, "")}</small></span>
              {server.auth === "none" ? <span className="pv-state">No key</span> : keyState(server)}
            </button>
          ))}
          {!words && <button type="button" className="pv-row" onClick={() => onChoose({ kind: "custom" })}>
            <span className="pv-tile"><Icon name="plus" /></span>
            <span className="pv-text"><b>Another server</b><small>Any address that speaks OpenAI or Gemini</small></span>
            <span className="pv-state">Type an address</span>
          </button>}
        </>}
      </div>}

      <div className="pv-foot">
        {unavailable.length > 0 ? <details className="pv-unavailable">
          <summary>{unavailable.length} providers need a connection Medha doesn't have yet</summary>
          <ul>{unavailable.map((provider) => <li key={provider.id}><b>{provider.name}</b>: {provider.reason || "Not available yet."}</li>)}</ul>
        </details> : <span />}
        <button type="button" className="cg-link" onClick={onClose}>{closeLabel}</button>
      </div>
    </div>
  );
}

/** What a pasted key is, read from its opening letters; nothing has been sent anywhere. */
function Recognition({ recognised, byId, pasted, onChoose, onBrowse }: {
  recognised: Recognised; byId: (id: string) => Listed | undefined; pasted: string; onChoose: (chosen: Chosen) => void; onBrowse: () => void;
}) {
  if (recognised.match === "unknown")
    return <p className="pv-note">Medha does not recognise this key from its opening letters. Pick its provider below; nothing is sent until you do.</p>;
  if (recognised.match === "one") {
    const found = recognised.providers[0];
    const provider = byId(found.id);
    return <div className="pv-found">
      <ProviderLogo id={found.id} name={found.name} size={22} />
      <span className="pv-text">
        <b>{/^[aeiou]/i.test(found.name) ? "An" : "A"} {found.name} key</b>
        <small>{!found.available ? found.reason ?? "Medha cannot connect to this provider yet." : recognised.warning ?? `Medha keeps it on this computer and sends it only to ${found.name}.`}</small>
      </span>
      {found.available && provider && <button type="button" className="btn-gold" onClick={() => onChoose({ kind: "provider", provider, key: pasted })}>Continue</button>}
      <button type="button" className="cg-link pv-other" onClick={onBrowse}>Not {found.name}?</button>
    </div>;
  }
  return <div className="pv-several">
    <p className="pv-ask"><b>Which provider is this key for?</b><small>Several providers use keys that start this way. Nothing is sent until you choose.</small></p>
    {recognised.providers.map((candidate) => {
      const provider = byId(candidate.id);
      return provider && <button type="button" key={candidate.id} className="pv-row" onClick={() => onChoose({ kind: "provider", provider, key: pasted })}>
        <ProviderLogo id={candidate.id} name={candidate.name} />
        <span className="pv-text"><b>{candidate.name}</b><small>{hostOf(provider.url)}</small></span>
        <span className="pv-state">Choose</span>
      </button>;
    })}
    <button type="button" className="pv-row" onClick={onBrowse}>
      <span className="pv-tile"><Icon name="search" /></span>
      <span className="pv-text"><b>Another provider</b><small>Search the full list</small></span>
      <span className="pv-state">Browse</span>
    </button>
    <p className="pv-note">Medha never tries a key on several providers.</p>
  </div>;
}
