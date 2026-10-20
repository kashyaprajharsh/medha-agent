import { Select } from "./Select";
import { useEffect, useRef, useState } from "react";
import { Health } from "./Health";
import { Keys } from "./Keys";
import { Usage } from "./Usage";
import { Memory } from "./Memory";
import { Add, PageTop, ViewBar } from "./ExtensionParts";
import { Icon } from "./Icon";
import { Sheet } from "./Sheet";
import { useWorkspace } from "./Workspace";
import { contextShort, plainName, priceLabel, profileFor, providerKey, reasonFor, sizeNote, type Found, type ConnectionOptions, type UnavailableProvider, type Provider as Preset, type Protocol as ModelProtocol, type ModelTestResult } from "./modelSetup";
import { ModelTest, ProviderFields } from "./ProviderSetup";
import { ChatGptAccounts, ChatGptSheet, ContinueWithChatGpt } from "./ChatGpt";
import { ConnectList, type Chosen, type Listed } from "./ConnectList";
import { ProviderLogo } from "./ProviderLogo";
import { Spinner } from "./Spinner";
export type Model = {
  name: string;
  profile: Record<string, unknown> & ConnectionOptions & {
    model: string;
    base_url: string;
    protocol: string;
    auth: string;
    provider_id?: string | null;
  };
  default: boolean;
  key_present: boolean;
};
type Search = {
  provider: string;
  searxng_url: string | null;
  key_present: boolean;
};
type Instruction = {
  kind: string;
  title: string;
  path: string;
  content: string;
  exists: boolean;
};
const hostOf = (url: string) => {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
};
/** Whether a model has the key it needs, in a few words. */
const keyOf = (model: Model) =>
  model.key_present
    ? { label: "Key saved", missing: false }
    : model.profile.auth === "chatgpt-plan"
      ? { label: "Uses your ChatGPT plan", missing: false }
      : model.profile.auth === "none"
        ? { label: "No key required", missing: false }
        : { label: "No saved key", missing: true };
// The instruction files that follow the person into every project.
const EVERYWHERE = ["persona", "global"];
const fileOf = (path: string) => path.split(/[\\/]/).at(-1) ?? path;
const PROVIDERS = [
  { id: "duckduckgo", name: "DuckDuckGo", note: "Keyless default. No API key needed." },
  { id: "tavily", name: "Tavily", note: "Search made for agents. Needs an API key." },
  { id: "brave", name: "Brave Search", note: "An independent index. Needs an API key." },
  { id: "searxng", name: "SearXNG", note: "Your own search server, at an address you give." },
];
const LOOKS = [
  { id: "soft", name: "Soft", note: "Raised, rounded surfaces. The default." },
  { id: "flat", name: "Flat", note: "Calm and quiet." },
  { id: "glass", name: "Glass", note: "Your desktop, blurred, behind the app." },
  { id: "neo", name: "Neo", note: "Ink outlines and hard shadows." },
];

export function Settings({
  sessionKey,
  ensureOpen,
  editModel,
  onEditedModel,
}: {
  sessionKey: string | null;
  ensureOpen: () => Promise<void>;
  editModel?: string;
  onEditedModel?: () => void;
}) {
  const context = useWorkspace();
  const api = context.api;
  const [protocols, setProtocols] = useState<ModelProtocol[]>([]);
  const [unavailable, setUnavailable] = useState<UnavailableProvider[]>([]);
  const [tab, setTab] = useState("models");
  const [models, setModels] = useState<Model[]>([]);
  const [standIns, setStandIns] = useState<string[]>([]);
  const [search, setSearch] = useState<Search>();
  const [instructions, setInstructions] = useState<Instruction[]>([]);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [editing, setEditing] = useState<Model | null>();
  const [busy, setBusy] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const [confirm, setConfirm] = useState<{ model: Model; key: boolean }>();
  const [opened, setOpened] = useState("");
  // Adding a model: what the list chose, counted so choosing again starts a fresh form.
  const [connect, setConnect] = useState<Chosen>();
  const [round, setRound] = useState(0);
  const choose = (chosen: Chosen) => {
    setConnect(chosen);
    setRound((value) => value + 1);
  };
  const [refreshing, setRefreshing] = useState(false);
  const [refreshError, setRefreshError] = useState("");
  const closeConnect = () => {
    setEditing(undefined);
    setConnect(undefined);
  };
  async function refreshCatalogue() {
    setRefreshing(true);
    setRefreshError("");
    try {
      await api.settings("application.providers.refresh", {});
      setRefresh((value) => value + 1);
    } catch (cause) {
      setRefreshError(String(cause).replace(/^Error:\s*/, ""));
    } finally {
      setRefreshing(false);
    }
  }
  // A ChatGPT sign-in in progress (`again` names a registered account), and the accounts sheet.
  const [chatgpt, setChatgpt] = useState<{ again?: string }>();
  const [chatgptAccounts, setChatgptAccounts] = useState(false);
  const open = tab === "models" && editing === undefined ? models.find((model) => model.name === opened) : undefined;
  // What follows is asked or done on the page, so the sheet steps aside for it.
  const aside = (then: () => void) => {
    setOpened("");
    then();
  };
  useEffect(() => {
    let active = true;
    setError("");
    void (
      tab === "instructions"
        ? api.settings("instructions.list")
        : api.settings()
    )
      .then((result) => {
        if (!active) return;
        if (tab === "instructions")
          setInstructions(result.files as Instruction[]);
        else {
          setModels(result.models as Model[]);
          setStandIns((result.fallback as string[] | undefined) ?? []);
          setProtocols(result.protocols as ModelProtocol[]);
          setUnavailable((result.unavailable_providers as UnavailableProvider[]) ?? []);
          setSearch(result.search as Search);
        }
      })
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [tab, refresh]);
  useEffect(() => {
    const asked = editModel && models.find((model) => model.name === editModel);
    if (!asked) return;
    setTab("models");
    setEditing(asked);
    onEditedModel?.();
  }, [editModel, models, onEditedModel]);
  async function act(method: string, params: Record<string, unknown>) {
    setBusy(true);
    setError("");
    setNotice("");
    try {
      await api.settings(method, params);
      setNotice("Saved");
      setEditing(undefined);
      setConfirm(undefined);
      setRefresh((value) => value + 1);
      // A saved model names itself so chats already using it can take the change.
      const profile = method === "settings.model.save" && typeof params.name === "string" && params.name ? params.name : undefined;
      window.dispatchEvent(new CustomEvent("medha-preferences", { detail: profile ? { profile } : undefined }));
    } catch (cause) {
      setError(String(cause));
      throw cause;
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="app-page">
      <div className="page-content">
        <div className="page-tabs" role="tablist" aria-label="Settings">
          {["models", "keys", "web search", "instructions", "memory", "usage", "appearance", "health"].map(
            (name) => (
              <button
                role="tab"
                aria-selected={tab === name}
                key={name}
                onClick={(event) => {
                  // Where the tabs slide sideways, the one picked comes fully into view.
                  event.currentTarget.scrollIntoView({ block: "nearest", inline: "nearest" });
                  setTab(name);
                  setEditing(undefined);
                  setNotice("");
                }}
              >
                {name[0].toUpperCase() + name.slice(1)}
              </button>
            ),
          )}
        </div>
        {error && (
          <p className="surface-error" role="alert">
            {error}
          </p>
        )}
        {notice && (
          <p className="quiet" role="status">
            {notice}
          </p>
        )}
        {tab === "health" && <Health />}
        {tab === "keys" && <Keys />}
        {tab === "usage" && <Usage />}
        {tab === "memory" && <Memory sessionKey={sessionKey} ensureOpen={ensureOpen} />}
        {tab === "models" && (
          <>
            <PageTop title="Models">
              The models you have saved. Pick one for a chat from the box you type in. Stand-ins take over, in number
              order, when a chat's model cannot answer.
            </PageTop>
            <ViewBar>
              <button type="button" className="cg-link" onClick={() => setChatgptAccounts(true)}>
                ChatGPT accounts
              </button>
              <ContinueWithChatGpt onClick={() => setChatgpt({})} />
              <Add
                label="Add model"
                open={editing === null}
                onClick={() => protocols.length && setEditing(editing === null ? undefined : null)}
              />
            </ViewBar>
            {chatgpt && (
              <ChatGptSheet
                again={chatgpt.again}
                onClose={() => setChatgpt(undefined)}
                onUseKey={() => {
                  setChatgpt(undefined);
                  setEditing(null);
                }}
                onSaved={(name) => {
                  setChatgpt(undefined);
                  setNotice("Saved");
                  setRefresh((value) => value + 1);
                  window.dispatchEvent(new CustomEvent("medha-preferences", { detail: { profile: name } }));
                }}
              />
            )}
            {chatgptAccounts && (
              <ChatGptAccounts
                onClose={() => setChatgptAccounts(false)}
                onSignIn={(again) => {
                  setChatgptAccounts(false);
                  setChatgpt({ again });
                }}
              />
            )}
            {editing !== undefined && (
              <Sheet label={editing ? `Edit ${editing.name}` : "Add model"} onClose={closeConnect} onBack={editing === null && connect ? () => setConnect(undefined) : undefined}>
                {editing === null && !connect ? (
                  <div className="cx-pick">
                    <h2>Add model</h2>
                    <ConnectList
                      protocols={protocols}
                      servers={ownServers(models, protocols)}
                      savedKeys={models.filter((item) => item.key_present).map((item) => item.profile.base_url)}
                      unavailable={unavailable}
                      refreshing={refreshing}
                      refreshError={refreshError}
                      onClose={closeConnect}
                      onRefresh={() => void refreshCatalogue()}
                      onChoose={(chosen) => {
                        if (chosen.kind === "chatgpt") {
                          closeConnect();
                          setChatgpt({});
                        } else choose(chosen);
                      }}
                    />
                  </div>
                ) : (
                  <ModelForm
                    key={editing?.name || `new-${round}`}
                    model={editing}
                    start={connect}
                    saved={models}
                    protocols={protocols}
                    busy={busy}
                    onCancel={closeConnect}
                    onBack={editing === null ? () => setConnect(undefined) : undefined}
                    onSwitch={(provider, key) => choose({ kind: "provider", provider, key })}
                    onSave={(params) => act("settings.model.save", params).then(() => setConnect(undefined))}
                  />
                )}
              </Sheet>
            )}
              <div className="ix">
                {models.map((model) => (
                  <div key={model.name} className="ix-row">
                    <button className="ix-main" onClick={() => setOpened(model.name)}>
                      <b>{model.name}</b>
                      <span className="ix-sum">
                        {model.profile.model} <i>on {hostOf(model.profile.base_url)}</i>
                      </span>
                    </button>
                    {model.default && <span className="ix-chip gold">Default</span>}
                    {standIns.includes(model.name) && (
                      <span className="ix-chip">Stand-in {standIns.indexOf(model.name) + 1}</span>
                    )}
                    <small className={`ix-note ${keyOf(model).missing ? "warn" : ""}`}>{keyOf(model).label}</small>
                  </div>
                ))}
                {!models.length && (
                  <p className="quiet">
                    Add your first model to start chatting.
                  </p>
                )}
              </div>
            {open && (
              <Sheet label={open.name} onClose={() => setOpened("")}>
                <div className="sheet-title">
                  <h2>{open.name}</h2>
                  {open.default && <span className="ix-chip gold">Default</span>}
                </div>
                <dl className="sheet-facts">
                  <dt>Model</dt>
                  <dd className="mono">{open.profile.model}</dd>
                  <dt>Endpoint</dt>
                  <dd className="mono">{open.profile.base_url}</dd>
                  <dt>Protocol</dt>
                  <dd>{protocols.find((protocol) => protocol.value === open.profile.protocol)?.label ?? open.profile.protocol}</dd>
                  <dt>Key</dt>
                  <dd className={keyOf(open).missing ? "warn" : ""}>{keyOf(open).label}</dd>
                  <dt>Stand-in</dt>
                  <dd>
                    {standIns.includes(open.name)
                      ? `Yes, position ${standIns.indexOf(open.name) + 1}`
                      : "No"}
                  </dd>
                </dl>
                <div className="sheet-actions">
                  <button className="btn-gold" onClick={() => aside(() => setEditing(open))}>
                    Edit
                  </button>
                  {!open.default && (
                    <button
                      className="btn-line"
                      disabled={busy}
                      onClick={() => void act("settings.model.default", { name: open.name }).catch(() => {})}
                    >
                      Make default
                    </button>
                  )}
                  <button
                    className="btn-line"
                    disabled={busy}
                    onClick={() =>
                      void act("settings.fallback.save", {
                        names: standIns.includes(open.name)
                          ? standIns.filter((name) => name !== open.name)
                          : [...standIns, open.name],
                      }).catch(() => {})
                    }
                  >
                    {standIns.includes(open.name) ? "Stop using as stand-in" : "Use as stand-in"}
                  </button>
                  {open.key_present && (
                    <button className="btn-line" onClick={() => aside(() => setConfirm({ model: open, key: true }))}>
                      Remove API key
                    </button>
                  )}
                  <button className="sheet-quiet" onClick={() => aside(() => setConfirm({ model: open, key: false }))}>
                    Remove model
                  </button>
                </div>
              </Sheet>
            )}
          </>
        )}
        {tab === "web search" && (
          <PageTop title="Web search">The search service Medha uses when it looks something up on the web.</PageTop>
        )}
        {tab === "web search" && search && (
          <SearchForm
            search={search}
            busy={busy}
            onSave={(params) => act("settings.search.save", params)}
          />
        )}
        {tab === "instructions" && (
          <>
            <PageTop title="Instructions">
              Persona and your instructions apply across projects. Project files apply in this folder. Changes load in
              a new chat.
            </PageTop>
            <InstructionEditor
              files={instructions}
              busy={busy}
              onSave={(params) => act("instructions.save", params)}
            />
          </>
        )}
        {tab === "appearance" && (
          <>
            <PageTop title="Appearance">How Medha looks on this computer. Changes show at once.</PageTop>
            <div className="appearance-row">
              <label htmlFor="theme">Theme</label>
              <Select
                id="theme"
                value={context.mode}
                onChange={(event) => context.setTheme(event.target.value)}
              >
                <option value="system">Match system</option>
                <option value="dark">Ink</option>
                <option value="light">Parchment</option>
              </Select>
            </div>
            <div className="appearance-looks">
              <span id="look-label">Style</span>
              <div role="radiogroup" aria-labelledby="look-label">
                {LOOKS.map((look) => (
                  <button
                    key={look.id}
                    role="radio"
                    aria-checked={context.look === look.id}
                    className="look-option"
                    onClick={() => context.setLook(look.id)}
                  >
                    <span className="look-sample" data-look={look.id}>
                      <i />
                      <i />
                      <b />
                    </span>
                    <strong>{look.name}</strong>
                    <small>
                      {look.id === "glass" &&
                      document.documentElement.dataset.platform !== "mac"
                        ? "Frosted menus and cards."
                        : look.note}
                    </small>
                  </button>
                ))}
              </div>
            </div>
            <div className="appearance-row">
              <label htmlFor="text-size">Reading size</label>
              <Select
                id="text-size"
                value={context.textSize}
                onChange={(event) => context.setTextSize(event.target.value)}
              >
                <option value="small">Small</option>
                <option value="default">Default</option>
                <option value="large">Large</option>
              </Select>
            </div>
            <p className="prose appearance-sample">
              Clear text, comfortable spacing. Code keeps its own monospace
              font.
            </p>
          </>
        )}
        {confirm && (
          <div className="dialog-backdrop">
            <section
              className="confirm-dialog"
              role="alertdialog"
              aria-modal="true"
              aria-labelledby="remove-title"
            >
              <h2 id="remove-title">
                Remove {confirm.key ? "API key" : "model"}?
              </h2>
              <p>
                {confirm.key
                  ? "This key is shared by models using the same endpoint. They may need a replacement key."
                  : `Remove ${confirm.model.name} from your saved models?`}
              </p>
              <div className="row-actions">
                <button
                  className="btn-line"
                  onClick={() => setConfirm(undefined)}
                >
                  Cancel
                </button>
                <button
                  className="btn-gold"
                  disabled={busy}
                  onClick={() =>
                    void act(
                      confirm.key
                        ? "settings.key.remove"
                        : "settings.model.remove",
                      { name: confirm.model.name },
                    ).catch(() => {})
                  }
                >
                  Remove
                </button>
              </div>
            </section>
          </div>
        )}
      </div>
    </div>
  );
}
/** Servers already in use that no catalogue provider covers, offered again so a second model needs no retyped address. */
export function ownServers(saved: Model[], protocols: ModelProtocol[]): Listed[] {
  const presets = protocols.filter((p) => p.available).flatMap((p) => p.providers.map((provider) => ({ ...provider, protocol: p.value })));
  const reused = [
    ...new Map(
      saved
        .filter((item) => !presets.some((listed) => listed.protocol === item.profile.protocol && listed.url === item.profile.base_url))
        .map((item) => [`${item.profile.protocol}:${item.profile.base_url}`, item.profile]),
    ),
  ];
  // Named by host alone unless two addresses share one.
  return reused.map(([, profile]) => ({
    name:
      reused.filter(([, other]) => hostOf(other.base_url) === hostOf(profile.base_url)).length > 1
        ? profile.base_url.replace(/^https?:\/\//, "")
        : hostOf(profile.base_url),
    url: profile.base_url,
    auth: profile.auth,
    protocol: profile.protocol,
    id: profile.provider_id ?? undefined,
    headers: profile.headers,
    ca_cert_file: profile.ca_cert_file,
    proxy: profile.proxy,
  }));
}

export function ModelForm({
  model,
  saved,
  protocols,
  start,
  busy,
  onSave,
  onCancel,
  onBack,
  onSwitch,
  firstRun,
}: {
  model: Model | null;
  saved: Model[];
  protocols: ModelProtocol[];
  /** What the list chose: a provider (with a pasted key) or a server of one's own. */
  start?: Chosen;
  busy: boolean;
  onSave: (params: Record<string, unknown>) => Promise<void>;
  onCancel: () => void;
  onBack?: () => void;
  /** Start again with the provider a typed key belongs to, keeping the key. */
  onSwitch: (provider: Listed, key: string) => void;
  /** The one model a new person starts with: always the default, "Start" to save, "Not now" to leave. */
  firstRun?: boolean;
}) {
  const api = useWorkspace().api;
  const form = useRef<HTMLFormElement>(null);
  const [name, setName] = useState(model?.name || "");
  const [id, setId] = useState(model?.profile.model || "");
  const [url, setUrl] = useState(model?.profile.base_url || "");
  const [protocol, setProtocol] = useState(model?.profile.protocol || "");
  const [providerId, setProviderId] = useState((model?.profile.provider_id as string | undefined) ?? "");
  // Saved deployments already have a resolved endpoint.
  const [needsParameters, setNeedsParameters] = useState(false);
  const availableProtocols = protocols;
  const selectedProtocol = availableProtocols.find((item) => item.value === protocol);
  const presets = availableProtocols.filter((p) => p.available).flatMap((p) => p.providers.map((provider) => ({ ...provider, protocol: p.value })));
  const own = ownServers(saved, availableProtocols);
  const serverKey = (server: Preset) => `${server.protocol}:${server.url}`;
  /** A key is stored per address, so any saved model there already holds it. */
  const keySavedAt = (address: string) =>
    saved.some((item) => item.key_present && item.profile.base_url === address);
  const [preset, setPreset] = useState(() => {
    if (!model) return "";
    const provider = protocols.find((item) => item.value === model.profile.protocol)?.providers.find((item) => (model.profile.provider_id && item.id === model.profile.provider_id) || item.url === model.profile.base_url);
    return provider ? providerKey({ ...provider, protocol: model.profile.protocol }) : "custom";
  });
  const [auth, setAuth] = useState(model?.profile.auth || "none");
  const [key, setKey] = useState("");
  const [isDefault, setDefault] = useState(model?.default || Boolean(firstRun));
  const [context, setContext] = useState(String(model?.profile.max_ctx || ""));
  const [output, setOutput] = useState(
    String(model?.profile.max_output_tokens || ""),
  );
  const [certificate, setCertificate] = useState(String(model?.profile.ca_cert_file || ""));
  const [proxy, setProxy] = useState(String(model?.profile.proxy || ""));
  const [reasoning, setReasoning] = useState(
    String(model?.profile.reasoning || "unknown"),
  );
  const [efforts, setEfforts] = useState<string[]>(
    (model?.profile.reasoning_efforts as string[]) || [],
  );
  const [images, setImages] = useState(
    String(model?.profile.image_input || "auto"),
  );
  const [counter, setCounter] = useState(
    String(model?.profile.token_counter || "none"),
  );
  const [accounting, setAccounting] = useState(
    String(model?.profile.token_accounting || "adaptive"),
  );
  const [tokenField, setTokenField] = useState(
    String(model?.profile.chat_token_limit || "auto"),
  );
  const [headers, setHeaders] = useState<[string, string][]>(
    Object.entries((model?.profile.headers as Record<string, string>) || {}),
  );
  const [discovered, setDiscovered] = useState<Found[]>([]);
  // A saved key is shown as saved; Change opens the field to replace it.
  const [changingKey, setChangingKey] = useState(false);
  // Found models are searched by ID or name, as the old picker allowed.
  const [modelQuery, setModelQuery] = useState("");
  // Before a key, the model ID field opens only when asked for.
  const [typingId, setTypingId] = useState(false);
  // Whether the context size was filled in from a listed model rather than typed.
  const listedContext = useRef(false);
  const [published, setPublished] = useState<{ model: string; size: number | null }>();
  const [discovering, setDiscovering] = useState(false);
  const [discoveryError, setDiscoveryError] = useState("");
  const [tested, setTested] = useState<ModelTestResult>();
  const [testing, setTesting] = useState(false);
  // A typed key that looks like another provider's: who it seems to belong to, and whether to send it anyway.
  const [elsewhere, setElsewhere] = useState<{ provider?: Listed; name: string; warning?: string }>();
  const [allowOther, setAllowOther] = useState(false);
  const provider = presets.find((p) => providerKey(p) === preset);
  const savedServer = own.find((item) => item.url === url && item.protocol === protocol);
  const discoveryVersion = useRef(0);
  const testVersion = useRef(0);
  const recogniseVersion = useRef(0);
  useEffect(() => {
    testVersion.current++;
    setTested(undefined); setTesting(false);
    return () => { testVersion.current++; };
  }, [id, url, protocol, providerId, key, auth, certificate, proxy, context, output, headers, reasoning, efforts, images, counter, accounting, tokenField]);
  useEffect(
    () => () => {
      discoveryVersion.current++;
      recogniseVersion.current++;
    },
    [],
  );
  // The list's choice opens here; a pasted key is the person's go-ahead to look for models.
  useEffect(() => {
    if (!start || model) return;
    if (start.kind === "custom") return chooseProvider(undefined, "custom");
    if (start.kind === "server") return chooseProvider(start.server, "custom");
    if (start.kind !== "provider") return;
    chooseProvider(start.provider, providerKey(start.provider));
    if (start.key) {
      setKey(start.key);
      const chosen = start.provider;
      void discover(profileFor({ ...chosen, protocol: chosen.protocol, auth: chosen.auth, provider_id: chosen.id }), start.key, chosen.models ?? []);
    }
  }, []); // eslint-disable-line react-hooks/exhaustive-deps
  // A whole key typed in is the go-ahead to list its models, unless it looks like another provider's.
  useEffect(() => {
    if (key.trim().length < 16 || elsewhere?.provider || !selectedProtocol?.discovery || !url) return;
    const wait = setTimeout(() => void discover(), 700);
    return () => clearTimeout(wait);
  }, [key, elsewhere]); // eslint-disable-line react-hooks/exhaustive-deps
  // A key typed for one provider is read before it is used: another provider's is held back.
  useEffect(() => {
    const mine = ++recogniseVersion.current;
    setElsewhere(undefined);
    setAllowOther(false);
    if (!key.trim() || preset === "custom") return;
    void api.settings("settings.model.recognise_key", { key }).then((reply) => {
      if (mine !== recogniseVersion.current) return;
      const found = (reply.providers as { id: string; name: string }[] | undefined)?.[0];
      const other = reply.match === "one" && found && found.id !== providerId;
      if (other) setElsewhere({ provider: presets.find((item) => item.id === found.id), name: found.name });
      else if (reply.warning) setElsewhere({ name: "", warning: reply.warning as string });
    }).catch(() => {});
  }, [key, providerId]); // eslint-disable-line react-hooks/exhaustive-deps
  function resetDiscovery() {
    discoveryVersion.current++;
    setDiscovered([]);
    setDiscoveryError("");
    setDiscovering(false);
    setTested(undefined);
    setTesting(false);
    testVersion.current++;
  }
  function resetConnection() {
    resetDiscovery();
    setKey("");
    setId("");
    setContext("");
    setOutput("");
    setReasoning("unknown");
    setEfforts([]);
    setImages("auto");
    setCounter("none");
    setAccounting("adaptive");
    setTokenField("auto");
    setHeaders([]);
    setCertificate("");
    setProxy("");
  }
  function chooseProvider(selected: Preset | undefined, choice: string) {
    const nextProtocol = selected?.protocol || protocol || availableProtocols.find((p) => p.available)?.value || "";
    const nextAuth = selected?.auth || availableProtocols.find((p) => p.value === nextProtocol)?.auth || "bearer";
    setProtocol(nextProtocol);
    setPreset(choice);
    setUrl(selected?.parameters?.length ? "" : selected?.url || "");
    setAuth(nextAuth);
    setProviderId(selected?.id ?? "");
    setNeedsParameters(Boolean(selected?.parameters?.length));
    resetConnection();
    setHeaders(Object.entries(selected?.headers ?? {}));
    setCertificate(selected?.ca_cert_file ?? "");
    setProxy(selected?.proxy ?? "");
    // Reuse only the selected address's saved key, never another provider's.
    if (selected && availableProtocols.find((p) => p.value === nextProtocol)?.discovery && !selected.parameters?.length && keySavedAt(selected.url))
      void discover(profileFor({ ...selected, protocol: nextProtocol, auth: nextAuth, provider_id: selected.id }), "", selected.models ?? []);
  }
  function profile() {
    return {
      ...model?.profile,
      model: id || "model-discovery",
      base_url: url,
      protocol,
      provider_id: providerId || null,
      auth,
      max_ctx: context ? Number(context) : null,
      max_output_tokens: output ? Number(output) : null,
      ca_cert_file: certificate.trim() || null,
      proxy: proxy.trim() || null,
      reasoning,
      reasoning_efforts: efforts.length ? efforts : null,
      image_input: images,
      token_counter: counter,
      token_accounting: accounting,
      chat_token_limit: tokenField,
      headers: Object.fromEntries(
        headers
          .filter(([name]) => name.trim())
          .map(([name, value]) => [name.trim(), value]),
      ),
    };
  }
  async function discover(from: object = profile(), secret = key, fallback = provider?.models ?? []) {
    const version = ++discoveryVersion.current;
    setDiscovering(true);
    setDiscovered([]);
    setDiscoveryError("");
    try {
      const result = await api.settings("settings.model.discover", {
        profile: from,
        key: secret,
        allow_other_provider: allowOther,
      });
      if (version !== discoveryVersion.current) return;
      const rows = result.models as typeof discovered;
      setDiscovered(rows);
      // The first found model is ticked, as the list shows it, unless one is already chosen.
      if (rows[0]) {
        setId((chosen) => chosen || rows[0].id);
        setContext((size) => {
          if (size || !rows[0].context_length) return size;
          listedContext.current = true;
          return String(rows[0].context_length);
        });
      }
      if (!rows.length)
        setDiscoveryError(
          "The server returned no models. You can enter a model ID below.",
        );
    } catch (cause) {
      if (version !== discoveryVersion.current) return;
      setDiscoveryError(
        `${reasonFor(cause, url)} ${fallback.length ? "Showing catalogue models; account availability is unverified." : "You can enter the model ID manually."}`,
      );
      setDiscovered(fallback);
    } finally {
      if (version === discoveryVersion.current) setDiscovering(false);
    }
  }
  async function testModel() {
    const version = ++testVersion.current;
    setTesting(true); setTested(undefined); setDiscoveryError("");
    try {
      const reply = await api.settings("settings.model.test", { profile: profile(), key, allow_other_provider: allowOther });
      if (version === testVersion.current) setTested(reply as ModelTestResult);
    } catch (cause) { if (version === testVersion.current) setDiscoveryError(String(cause)); }
    finally { if (version === testVersion.current) setTesting(false); }
  }
  const title = model
    ? `Edit ${model.name}`
    : preset === "custom"
      ? (savedServer?.name ?? "Another server")
      : plainName(provider?.name ?? "Add model");
  // Who this is about: the provider or server, even while editing a saved model.
  const brand = plainName(provider?.name ?? savedServer?.name ?? (url ? hostOf(url) : "your server"));
  const ownServer = preset === "custom" && !providerId;
  const needsKey = auth !== "none" && !key.trim() && !keySavedAt(url);
  // A model with no size given: say what Medha will use instead, from the published size.
  const unsized = !context && id.trim() ? id.trim() : "";
  useEffect(() => {
    if (!unsized || !protocol || !url) return;
    let current = true;
    const timer = window.setTimeout(() => {
      void api
        .settings("settings.model.context", { profile: profileFor({ protocol, url, auth: "none", provider_id: providerId || undefined }, unsized) })
        .then((reply) => current && setPublished({ model: unsized, size: (reply.published as number | null) ?? null }))
        .catch(() => {});
    }, 300);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [api, unsized, protocol, url, providerId]);
  const modelId = (
    <label>
      Model ID
      <input
        required
        value={id}
        placeholder="The model ID your provider uses"
        onChange={(event) => {
          setId(event.target.value);
          // A size taken from the list belongs to that listed model, not to one typed by hand.
          if (listedContext.current) {
            listedContext.current = false;
            setContext("");
          }
        }}
      />
    </label>
  );
  const speaks = (
    <label>
      Speaks
      <Select
        value={protocol}
        disabled={busy}
        onChange={(event) => {
          const next = availableProtocols.find((item) => item.value === event.target.value);
          if (!next?.available) return;
          setProtocol(next.value);
          setPreset(preset === "custom" ? "custom" : "");
          setAuth(next.auth);
          setProviderId("");
          resetConnection();
        }}
      >
        <option value="" disabled>
          Choose a protocol
        </option>
        {availableProtocols
          .filter((item) => item.available || item.value === protocol)
          .map((item) => (
            <option key={item.value} value={item.value} disabled={!item.available}>
              {item.label}
            </option>
          ))}
      </Select>
    </label>
  );
  const passed = tested && (["text", "streaming", "tools"] as const).every((check) => tested[check].status === "passed");
  const endpoint = (
    <>
      <label>
        API endpoint
        <input
          required
          type="url"
          value={url}
          placeholder="https://your-server.example/v1"
          onChange={(event) => {
            setUrl(event.target.value);
            resetDiscovery();
          }}
        />
      </label>
      <p className="quiet">
        {preset === "custom" ? "Enter your server’s base URL." : "Filled in for this provider. You can change it for your server."}
      </p>
    </>
  );
  return (
    <form
      ref={form}
      className="settings-form cx-form"
      onSubmit={(event) => {
        event.preventDefault();
        if (!protocol || !preset || !selectedProtocol?.available) return;
        void onSave({ name, profile: profile(), key, default: isDefault, allow_other_provider: allowOther })
          .then(() => setKey(""))
          .catch(() => {});
      }}
    >
      {onBack && <button type="button" className="cx-back" onClick={onBack}>‹ All providers</button>}
      <div className="cg-who">
        {ownServer ? (
          <span className="pv-tile" aria-hidden="true"><Icon name="globe" /></span>
        ) : (
          <ProviderLogo id={providerId || undefined} name={brand} size={24} />
        )}
        <div><h2>{title}</h2>{url && <small className="cx-address">{url.replace(/^https?:\/\//, "").replace(/\/$/, "")}</small>}</div>
      </div>
      {!preset && speaks}
      {protocol && preset && (
        <>
          {needsParameters && provider?.parameters?.length ? <ProviderFields key={providerKey(provider)} provider={provider} disabled={busy} onChange={() => { setUrl(""); setKey(""); resetDiscovery(); }} onResolved={(url) => { setUrl(url); resetDiscovery(); }} /> : null}
          {preset === "custom" && own.length > 0 && <>
            <label>
              Saved servers
              <Select value={savedServer ? serverKey(savedServer) : "new"} search="Search saved servers" disabled={busy} onChange={(event) => chooseProvider(own.find((item) => serverKey(item) === event.target.value), "custom")}>
                <option value="new">Enter a new endpoint</option>
                {own.map((item) => <option key={serverKey(item)} value={serverKey(item)}>{item.name}</option>)}
              </Select>
            </label>
            <p className="quiet">Only endpoints from your saved models appear here.</p>
          </>}
          {preset === "custom" && endpoint}
          {preset === "custom" && speaks}
          {auth !== "none" && keySavedAt(url) && !changingKey ? (
            <>
              <p className="cx-field-label"><span>Key</span><button type="button" className="cg-link" onClick={() => setChangingKey(true)}>Change</button></p>
              <div className="cx-keyline"><span className="cg-dot" /><span className="grow">Saved on this computer</span><span className="quiet">Sent only to {hostOf(url)}</span></div>
            </>
          ) : auth !== "none" && (
            <>
              <label>
                <span className="cx-label-row">
                  <span>Key</span>
                  {changingKey && (
                    <button type="button" className="cg-link" onClick={() => { setChangingKey(false); setKey(""); }}>
                      Keep saved key
                    </button>
                  )}
                </span>
                <input
                  type="password"
                  autoComplete="new-password"
                  value={key}
                  autoFocus={changingKey}
                  placeholder={ownServer ? "API key, if it needs one" : keySavedAt(url) ? "Leave empty to use the saved key" : `Paste your ${brand} key`}
                  onChange={(event) => {
                    setKey(event.target.value);
                    resetDiscovery();
                  }}
                />
              </label>
              <p className="quiet">
                {ownServer
                  ? "Leave it empty for a server on this computer."
                  : `Medha keeps this key on this computer and sends it only to ${hostOf(url) || brand}.`}
              </p>
            </>
          )}
          {elsewhere?.provider && !allowOther && (
            <div className="cx-warn" role="alert">
              <ProviderLogo id={elsewhere.provider.id} name={elsewhere.name} />
              <div><b>This looks like a {elsewhere.name} key</b><p>It was not sent to {title}.</p></div>
              <span className="cg-actions">
                <button type="button" className="btn-gold" onClick={() => elsewhere.provider && onSwitch(elsewhere.provider, key)}>Use {elsewhere.name}</button>
                <button type="button" className="btn-line" onClick={() => setAllowOther(true)}>Use it anyway</button>
              </span>
            </div>
          )}
          {elsewhere?.warning && <p className="cx-note" role="status">{elsewhere.warning}</p>}
          <div className="model-discovery cx-field-label">
            <span>Model</span>
            <span className="cg-actions">
              {discovered.some((item) => item.tools != null) && (
                <span className="quiet">{discovered.filter((item) => item.tools).length} that can use tools</span>
              )}
              {discovered.length > 0 && selectedProtocol?.discovery && (
                <button
                  type="button"
                  className="icon-btn"
                  aria-label="Find models on this server"
                  title="Find models on this server again"
                  disabled={busy || discovering || !url || needsKey}
                  onClick={() => void discover()}
                >
                  <Icon name="refresh" />
                </button>
              )}
            </span>
          </div>
          {discovering && (
            <p className="cx-finding" role="status"><Spinner />Finding {brand}’s models…</p>
          )}
          {!discovering && !discovered.length && selectedProtocol?.discovery && (
            needsKey ? (
              <p className="quiet">
                Paste your key to see {brand}’s models.{" "}
                {!typingId && <button type="button" className="cg-link" onClick={() => setTypingId(true)}>Or type a model ID</button>}
              </p>
            ) : (
              <button type="button" className="btn-line cx-find" disabled={busy || !url} onClick={() => void discover()}>
                Find models on this server
              </button>
            )
          )}
          {discoveryError && (
            <p className="quiet" role="status">
              {discoveryError}
            </p>
          )}
          {discovered.length > 0 && (
            <input
              className="cx-model-search"
              aria-label="Search models"
              placeholder={`Search ${discovered.length} models`}
              value={modelQuery}
              spellCheck={false}
              autoComplete="off"
              onChange={(event) => setModelQuery(event.target.value)}
            />
          )}
          {discovered.length > 0 && (
            <div className="cx-models" role="listbox" aria-label="Models">
              <div className="cx-model head" aria-hidden="true"><span /><span>Name</span><span>Context</span><span>Per 1M, in / out</span></div>
              {discovered.filter((item) => `${item.id} ${item.name ?? ""}`.toLowerCase().includes(modelQuery.trim().toLowerCase())).map((item) => (
                <button
                  type="button"
                  role="option"
                  aria-selected={id === item.id}
                  key={item.id}
                  data-model={item.id}
                  className={`cx-model${id === item.id ? " on" : ""}`}
                  onClick={() => {
                    setId(item.id);
                    setContext(item.context_length ? String(item.context_length) : "");
                    listedContext.current = Boolean(item.context_length);
                  }}
                  onDoubleClick={() => form.current?.requestSubmit()}
                >
                  <span className="tick">{id === item.id ? "✓" : ""}</span>
                  <b>{item.name || item.id}</b>
                  <span>{contextShort(item.context_length)}</span>
                  <span>{priceLabel(item)}</span>
                </button>
              ))}
            </div>
          )}
          {!discovered.length && (!needsKey || typingId || !selectedProtocol?.discovery || id) && modelId}
          <ModelTest result={tested} />
          <details className="cx-advanced">
            <summary><span>Name, limits and advanced settings</span><span aria-hidden="true">›</span></summary>
            {discovered.length > 0 && modelId}
            <label>
              Name{!model && " (optional)"}
              <input
                disabled={Boolean(model)}
                value={name}
                placeholder="Named automatically from the model ID"
                onChange={(event) => setName(event.target.value)}
              />
            </label>
            <label>
              Context window (tokens)
              <input
                type="number"
                min="1"
                max="4294967295"
                step="1"
                value={context}
                placeholder="Use provider metadata"
                onChange={(event) => {
                  setContext(event.target.value);
                  listedContext.current = false;
                }}
              />
            </label>
            <p className="quiet">
              {unsized && published?.model === unsized.trim()
                ? sizeNote(discovered.some((item) => item.id === unsized), published.size)
                : "For local models, use the context window configured on your server."}
            </p>
            {preset !== "custom" && endpoint}
            {model && preset !== "custom" && speaks}
            <label>
              Maximum output tokens
              <input
                type="number"
                min="1"
                step="1"
                value={output}
                placeholder="Provider default"
                onChange={(event) => setOutput(event.target.value)}
              />
            </label>
            <label>
              Certificate file
              <input
                value={certificate}
                spellCheck={false}
                placeholder="Only for a company proxy or a self-signed server"
                onChange={(event) => {
                  setCertificate(event.target.value);
                  resetDiscovery();
                }}
              />
            </label>
            <p className="quiet">
              The full path of a .pem file with the certificate this server is signed by. Medha then trusts it for
              this server, as well as the usual public ones.
            </p>
            <label>
              Model proxy
              <input value={proxy} spellCheck={false} placeholder="Off, or https://proxy.example:8443"
                onChange={(event) => { setProxy(event.target.value); resetDiscovery(); }} />
            </label>
            <p className="quiet">This saved model sends its requests through this proxy, including authenticated traffic. A TLS-inspecting proxy also needs its certificate above. Web tools and servers use their own connections.</p>
            <label>
              Authentication
              <Select
                value={auth}
                onChange={(event) => {
                  setAuth(event.target.value);
                  resetDiscovery();
                }}
              >
                {[
                  { value: "none", label: "No key" },
                  { value: "bearer", label: "Bearer token" },
                  { value: "x-api-key", label: "API key header" },
                  { value: "x-goog-api-key", label: "Google API key header" },
                ].map((item) => (
                  <option key={item.value} value={item.value}>
                    {item.label}
                  </option>
                ))}
              </Select>
            </label>
            <label>
              Reasoning support
              <Select
                value={reasoning}
                onChange={(event) => setReasoning(event.target.value)}
              >
                <option value="unknown">Discover automatically</option>
                <option value="unsupported">No reasoning controls</option>
                <option value="effort">Supports effort levels</option>
              </Select>
            </label>
            {reasoning === "effort" && (
              <fieldset>
                <legend>Supported effort levels</legend>
                {[
                  "none",
                  "minimal",
                  "low",
                  "medium",
                  "high",
                  "xhigh",
                  "max",
                ].map((level) => (
                  <label className="checkbox-label" key={level}>
                    <input
                      type="checkbox"
                      checked={efforts.includes(level)}
                      onChange={(event) =>
                        setEfforts(
                          event.target.checked
                            ? [...efforts, level]
                            : efforts.filter((value) => value !== level),
                        )
                      }
                    />
                    {level}
                  </label>
                ))}
              </fieldset>
            )}
            <label>
              Image input
              <Select
                value={images}
                onChange={(event) => setImages(event.target.value)}
              >
                <option value="auto">Automatic</option>
                <option value="native">Send images directly</option>
                <option value="text">Use image descriptions</option>
              </Select>
            </label>
            <label>
              Token counting
              <Select
                value={counter}
                onChange={(event) => setCounter(event.target.value)}
              >
                <option value="none">Automatic</option>
                <option value="vllm">vLLM server</option>
              </Select>
            </label>
            <label>
              When the count is uncertain
              <Select
                value={accounting}
                onChange={(event) => setAccounting(event.target.value)}
              >
                <option value="adaptive">Adapt</option>
                <option value="strict">Be strict</option>
              </Select>
            </label>
            {protocol === "open-ai-chat" && (
              <label>
                Output limit field
                <Select
                  value={tokenField}
                  onChange={(event) => setTokenField(event.target.value)}
                >
                  <option value="auto">Automatic</option>
                  <option value="max_tokens">max_tokens</option>
                  <option value="max_completion_tokens">
                    max_completion_tokens
                  </option>
                </Select>
              </label>
            )}
            <h4>Additional headers</h4>
            {headers.map(([name, value], index) => (
              <div className="header-pair" key={index}>
                <input
                  aria-label="Header name"
                  value={name}
                  onChange={(event) =>
                    setHeaders(
                      headers.map((pair, at) =>
                        at === index ? [event.target.value, pair[1]] : pair,
                      ),
                    )
                  }
                />
                <input
                  aria-label="Header value"
                  value={value}
                  onChange={(event) =>
                    setHeaders(
                      headers.map((pair, at) =>
                        at === index ? [pair[0], event.target.value] : pair,
                      ),
                    )
                  }
                />
                <button
                  type="button"
                  className="icon-btn"
                  aria-label="Remove header"
                  onClick={() =>
                    setHeaders(headers.filter((_, at) => at !== index))
                  }
                >
                  <Icon name="x" />
                </button>
              </div>
            ))}
            <button
              type="button"
              className="btn-line"
              onClick={() => setHeaders([...headers, ["", ""]])}
            >
              Add header
            </button>
            <p className="quiet">Use the API key field for credentials.</p>
          </details>
          {!firstRun && <label className="checkbox-label">
            <input
              type="checkbox"
              checked={isDefault}
              onChange={(event) => setDefault(event.target.checked)}
            />
            Use as the default model for new chats
          </label>}
        </>
      )}
      <div className="cx-formfoot">
        {passed ? (
          <span className="cx-passed"><span className="cg-dot" />Replies, streams and calls tools</span>
        ) : (
          <p className="quiet">
            {testing
              ? "Checking. Up to 5 minutes per request, 8 minutes in total."
              : "Sends up to four small test requests, which your provider may bill."}
          </p>
        )}
        <span className="cg-actions">
          {(model || firstRun) && <button type="button" className="cx-ghost" onClick={onCancel}>{firstRun ? "Not now" : "Cancel"}</button>}
          {protocol && preset && (
            <>
              <button type="button" className="cx-ghost" disabled={busy || testing || discovering || !id.trim() || !url || (auth !== "none" && !key.trim() && !keySavedAt(url))} onClick={() => void testModel()}>{testing ? "Checking…" : tested ? "Check again" : "Check connection"}</button>
              <button className="btn-gold" disabled={busy || !selectedProtocol?.available}>
                {busy ? "Saving…" : model ? "Save" : firstRun ? "Start" : "Add model"}
              </button>
            </>
          )}
        </span>
      </div>
    </form>
  );
}
function SearchForm({
  search,
  busy,
  onSave,
}: {
  search: Search;
  busy: boolean;
  onSave: (params: Record<string, unknown>) => Promise<void>;
}) {
  const [provider, setProvider] = useState(search.provider);
  const [url, setUrl] = useState(search.searxng_url || "");
  const [key, setKey] = useState("");
  return (
    <form
      className="settings-form plain"
      onSubmit={(event) => {
        event.preventDefault();
        void onSave({ provider, url, key })
          .then(() => setKey(""))
          .catch(() => {});
      }}
    >
      <div className="ix" role="radiogroup" aria-label="Provider">
        {PROVIDERS.map((each) => (
          <button
            type="button"
            role="radio"
            key={each.id}
            aria-checked={provider === each.id}
            className="ix-row pick"
            onClick={() => {
              setProvider(each.id);
              setKey("");
            }}
          >
            <span className="option-radio" />
            <span className="ix-main">
              <b>{each.name}</b>
              <span className="ix-sum">{each.note}</span>
            </span>
            {search.provider === each.id && <span className="ix-chip gold">In use</span>}
          </button>
        ))}
      </div>
      {provider === "searxng" && (
        <label>
          Server URL
          <input
            required
            type="url"
            value={url}
            onChange={(event) => setUrl(event.target.value)}
          />
        </label>
      )}
      {["tavily", "brave"].includes(provider) && (
        <label>
          API key
          <input
            type="password"
            autoComplete="new-password"
            value={key}
            onChange={(event) => setKey(event.target.value)}
            placeholder={
              search.provider === provider && search.key_present
                ? "Leave empty to keep saved key"
                : "Paste API key"
            }
          />
        </label>
      )}
      <button className="btn-gold" disabled={busy}>
        Save search provider
      </button>
    </form>
  );
}
function InstructionEditor({
  files,
  busy,
  onSave,
}: {
  files: Instruction[];
  busy: boolean;
  onSave: (params: Record<string, unknown>) => Promise<void>;
}) {
  const [kind, setKind] = useState("persona");
  const file = files.find((item) => item.kind === kind);
  const [content, setContent] = useState("");
  useEffect(() => {
    setContent(file?.content || "");
  }, [file]);
  return (
    <div className="instruction-editor">
      <nav className="instruction-files" aria-label="Instructions file">
        {[
          { title: "Everywhere", rows: files.filter((item) => EVERYWHERE.includes(item.kind)) },
          { title: "This project", rows: files.filter((item) => !EVERYWHERE.includes(item.kind)) },
        ].map(
          (group) =>
            group.rows.length > 0 && (
              <section key={group.title}>
                <h3>{group.title}</h3>
                {group.rows.map((item) => (
                  <button key={item.kind} aria-current={kind === item.kind} onClick={() => setKind(item.kind)}>
                    <i className={item.exists ? "written" : ""} />
                    <span>
                      <b>{item.title}</b>
                      <small>{item.exists ? fileOf(item.path) : "Not written yet"}</small>
                    </span>
                  </button>
                ))}
              </section>
            ),
        )}
      </nav>
      <div className="instruction-page">
        <textarea
          aria-label="Instructions"
          spellCheck={false}
          value={content}
          onChange={(event) => setContent(event.target.value)}
          placeholder="Write the guidance Medha should follow…"
        />
        <div className="instruction-foot">
          <button
            className="btn-gold"
            disabled={busy || !file || content === file.content}
            onClick={() =>
              void onSave({ kind, content, before: file?.content }).catch(() => {})
            }
          >
            Save instructions
          </button>
          <code title={file?.path}>{file?.path}</code>
        </div>
      </div>
    </div>
  );
}
