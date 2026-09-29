import { Select } from "./Select";
import { useEffect, useRef, useState } from "react";
import { Health } from "./Health";
import { Keys } from "./Keys";
import { Usage } from "./Usage";
import { Memory } from "./Memory";
import { Icon } from "./Icon";
import { useWorkspace } from "./Workspace";
type Model = {
  name: string;
  profile: Record<string, unknown> & {
    model: string;
    base_url: string;
    protocol: string;
    auth: string;
  };
  default: boolean;
  key_present: boolean;
};
type Preset = { name: string; url: string; auth: string };
type ModelProtocol = {
  value: string;
  label: string;
  available: boolean;
  auth: string;
  discovery: boolean;
  providers: Preset[];
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
const LOOKS = [
  { id: "soft", name: "Soft", note: "Raised, rounded surfaces. The default." },
  { id: "flat", name: "Flat", note: "Calm and quiet." },
  { id: "glass", name: "Glass", note: "Your desktop, blurred, behind the app." },
  { id: "neo", name: "Neo", note: "Ink outlines and hard shadows." },
];

export function Settings({
  sessionKey,
  ensureOpen,
}: {
  sessionKey: string | null;
  ensureOpen: () => Promise<void>;
}) {
  const context = useWorkspace();
  const api = context.api;
  const [protocols, setProtocols] = useState<ModelProtocol[]>([]);
  const [tab, setTab] = useState("models");
  const [models, setModels] = useState<Model[]>([]);
  const [search, setSearch] = useState<Search>();
  const [instructions, setInstructions] = useState<Instruction[]>([]);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [editing, setEditing] = useState<Model | null>();
  const [busy, setBusy] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const [confirm, setConfirm] = useState<{ model: Model; key: boolean }>();
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
          setProtocols(result.protocols as ModelProtocol[]);
          setSearch(result.search as Search);
        }
      })
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [tab, refresh]);
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
      window.dispatchEvent(new CustomEvent("medha-preferences"));
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
                onClick={() => {
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
            <div className="page-heading">
              <div>
                <h2>Models</h2>
                <p>Saved models are available in the chat composer.</p>
              </div>
              <button
                className="btn-line"
                disabled={!protocols.length}
                onClick={() => setEditing(null)}
              >
                <Icon name="plus" />
                Add model
              </button>
            </div>
            {editing !== undefined ? (
              <ModelForm
                key={editing?.name || "new"}
                model={editing}
                protocols={protocols}
                busy={busy}
                onCancel={() => setEditing(undefined)}
                onSave={(params) => act("settings.model.save", params)}
              />
            ) : (
              <div className="settings-list">
                {models.map((model) => (
                  <section key={model.name} className="settings-row">
                    <div>
                      <b>
                        {model.name}
                        {model.default && (
                          <span className="default-label">Default</span>
                        )}
                      </b>
                      <p>{model.profile.model}</p>
                      <small>
                        {model.profile.base_url} ·{" "}
                        {model.key_present
                          ? "Key saved"
                          : model.profile.auth === "none"
                            ? "No key required"
                            : "No saved key"}
                      </small>
                    </div>
                    <div className="row-actions">
                      <button
                        className="btn-line"
                        onClick={() => setEditing(model)}
                      >
                        Edit
                      </button>
                      <details className="row-more">
                        <summary aria-label={`More actions for ${model.name}`}>
                          •••
                        </summary>
                        <div>
                          {!model.default && (
                            <button
                              disabled={busy}
                              onClick={() =>
                                void act("settings.model.default", {
                                  name: model.name,
                                }).catch(() => {})
                              }
                            >
                              Make default
                            </button>
                          )}
                          {model.key_present && (
                            <button
                              onClick={() => setConfirm({ model, key: true })}
                            >
                              Remove API key
                            </button>
                          )}
                          <button
                            onClick={() => setConfirm({ model, key: false })}
                          >
                            Remove model
                          </button>
                        </div>
                      </details>
                    </div>
                  </section>
                ))}
                {!models.length && (
                  <p className="quiet">
                    Add your first model to start chatting.
                  </p>
                )}
              </div>
            )}
          </>
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
            <div className="page-heading">
              <div>
                <h2>Instructions</h2>
                <p>
                  Persona and your instructions apply across projects. Project
                  files apply in this folder. Changes load in a new chat.
                </p>
              </div>
            </div>
            <InstructionEditor
              files={instructions}
              busy={busy}
              onSave={(params) => act("instructions.save", params)}
            />
          </>
        )}
        {tab === "appearance" && (
          <>
            <h2>Appearance</h2>
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
function ModelForm({
  model,
  protocols,
  busy,
  onSave,
  onCancel,
}: {
  model: Model | null;
  protocols: ModelProtocol[];
  busy: boolean;
  onSave: (params: Record<string, unknown>) => Promise<void>;
  onCancel: () => void;
}) {
  const api = useWorkspace().api;
  const [name, setName] = useState(model?.name || "");
  const [id, setId] = useState(model?.profile.model || "");
  const [url, setUrl] = useState(model?.profile.base_url || "");
  const [protocol, setProtocol] = useState(model?.profile.protocol || "");
  const selectedProtocol = protocols.find((item) => item.value === protocol);
  const presets = selectedProtocol?.providers || [];
  const [preset, setPreset] = useState(() =>
    model
      ? protocols
          .find((item) => item.value === model.profile.protocol)
          ?.providers.find((item) => item.url === model.profile.base_url)
          ?.url || "custom"
      : "",
  );
  const [auth, setAuth] = useState(model?.profile.auth || "none");
  const [key, setKey] = useState("");
  const [isDefault, setDefault] = useState(model?.default || false);
  const [context, setContext] = useState(String(model?.profile.max_ctx || ""));
  const [output, setOutput] = useState(
    String(model?.profile.max_output_tokens || ""),
  );
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
  const [discovered, setDiscovered] = useState<
    { id: string; context_length: number | null }[]
  >([]);
  const [discovering, setDiscovering] = useState(false);
  const [discoveryError, setDiscoveryError] = useState("");
  const discoveryVersion = useRef(0);
  useEffect(
    () => () => {
      discoveryVersion.current++;
    },
    [],
  );
  function resetDiscovery() {
    discoveryVersion.current++;
    setDiscovered([]);
    setDiscoveryError("");
    setDiscovering(false);
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
  }
  function profile() {
    return {
      ...model?.profile,
      model: id || "model-discovery",
      base_url: url,
      protocol,
      auth,
      max_ctx: context ? Number(context) : null,
      max_output_tokens: output ? Number(output) : null,
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
  async function discover() {
    const version = ++discoveryVersion.current;
    setDiscovering(true);
    setDiscovered([]);
    setDiscoveryError("");
    try {
      const result = await api.settings("settings.model.discover", {
        profile: profile(),
        key,
      });
      if (version !== discoveryVersion.current) return;
      const rows = result.models as typeof discovered;
      setDiscovered(rows);
      if (!rows.length)
        setDiscoveryError(
          "The server returned no models. You can enter a model ID below.",
        );
    } catch (cause) {
      if (version !== discoveryVersion.current) return;
      setDiscoveryError(
        `${String(cause)} You can enter the model ID manually.`,
      );
    } finally {
      if (version === discoveryVersion.current) setDiscovering(false);
    }
  }
  return (
    <form
      className="settings-form"
      onSubmit={(event) => {
        event.preventDefault();
        if (!protocol || !preset || !selectedProtocol?.available) return;
        void onSave({ name, profile: profile(), key, default: isDefault })
          .then(() => setKey(""))
          .catch(() => {});
      }}
    >
      <h3>{model ? `Edit ${model.name}` : "Add model"}</h3>
      <label>
        Protocol
        <Select
          value={protocol}
          disabled={busy}
          onChange={(event) => {
            const next = protocols.find(
              (item) => item.value === event.target.value,
            );
            if (!next?.available) return;
            setProtocol(next.value);
            setPreset("");
            setUrl("");
            setAuth(next.auth);
            resetConnection();
          }}
        >
          <option value="" disabled>
            Choose a protocol
          </option>
          {protocols
            .filter((item) => item.available || item.value === protocol)
            .map((item) => (
              <option
                key={item.value}
                value={item.value}
                disabled={!item.available}
              >
                {item.label}
              </option>
            ))}
        </Select>
      </label>
      {protocol && (
        <label>
          Provider
          <Select
            value={preset}
            disabled={busy || !selectedProtocol?.available}
            onChange={(event) => {
              const selected = presets.find(
                (item) => item.url === event.target.value,
              );
              setPreset(event.target.value);
              setUrl(selected?.url || "");
              setAuth(selected?.auth || selectedProtocol?.auth || "bearer");
              resetConnection();
            }}
          >
            <option value="" disabled>
              Choose a provider
            </option>
            {presets.map((item) => (
              <option key={item.url} value={item.url}>
                {item.name}
              </option>
            ))}
            <option value="custom">Custom endpoint</option>
          </Select>
        </label>
      )}
      {protocol && preset && (
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
            {preset === "custom"
              ? "Enter your server’s base URL."
              : "Filled from the provider preset. You can change it for your server."}
          </p>
          {auth !== "none" && (
            <label>
              API key
              <input
                type="password"
                autoComplete="new-password"
                value={key}
                placeholder={
                  model?.key_present
                    ? "Leave empty to keep saved key"
                    : "Paste API key"
                }
                onChange={(event) => {
                  setKey(event.target.value);
                  resetDiscovery();
                }}
              />
            </label>
          )}
          {selectedProtocol?.discovery && (
            <div className="model-discovery">
              <button
                type="button"
                className="btn-line"
                disabled={
                  busy ||
                  discovering ||
                  !url ||
                  (auth !== "none" &&
                    !key.trim() &&
                    !(model?.key_present && url === model.profile.base_url))
                }
                onClick={() => void discover()}
              >
                {discovering ? "Finding models…" : "Find models on this server"}
              </button>
            </div>
          )}
          {discoveryError && (
            <p className="quiet" role="status">
              {discoveryError}
            </p>
          )}
          {discovered.length > 0 && (
            <label>
              Available models
              <Select
                value={id}
                onChange={(event) => {
                  const selected = discovered.find(
                    (item) => item.id === event.target.value,
                  )!;
                  setId(selected.id);
                  if (!name)
                    setName(selected.id.replace(/[^a-zA-Z0-9._-]/g, "-"));
                  if (selected.context_length)
                    setContext(String(selected.context_length));
                }}
              >
                <option value="">Choose a model</option>
                {discovered.map((item) => (
                  <option key={item.id} value={item.id}>
                    {item.id}
                  </option>
                ))}
              </Select>
            </label>
          )}
          <label>
            Model ID
            <input
              required
              value={id}
              placeholder="Model ID from your provider"
              onChange={(event) => setId(event.target.value)}
            />
          </label>
          <label>
            Profile name
            <input
              required
              disabled={Boolean(model)}
              value={name}
              placeholder="my-model"
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
              onChange={(event) => setContext(event.target.value)}
            />
          </label>
          <p className="quiet">
            For local models, use the context window configured on your server.
          </p>
          <details>
            <summary>Advanced model settings</summary>
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
              Token accounting
              <Select
                value={accounting}
                onChange={(event) => setAccounting(event.target.value)}
              >
                <option value="adaptive">Adaptive</option>
                <option value="strict">Strict</option>
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
          <label className="checkbox-label">
            <input
              type="checkbox"
              checked={isDefault}
              onChange={(event) => setDefault(event.target.checked)}
            />
            Use as default
          </label>
        </>
      )}
      <div className="row-actions">
        <button type="button" className="btn-line" onClick={onCancel}>
          Cancel
        </button>
        {protocol && preset && (
          <button
            className="btn-gold"
            disabled={busy || !selectedProtocol?.available}
          >
            {busy ? "Saving…" : "Save model"}
          </button>
        )}
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
      className="settings-form"
      onSubmit={(event) => {
        event.preventDefault();
        void onSave({ provider, url, key })
          .then(() => setKey(""))
          .catch(() => {});
      }}
    >
      <h2>Web search</h2>
      <p>Choose the search service Medha uses for web research.</p>
      <label>
        Provider
        <Select
          value={provider}
          onChange={(event) => {
            setProvider(event.target.value);
            setKey("");
          }}
        >
          {["duckduckgo", "tavily", "brave", "searxng"].map((value) => (
            <option key={value}>{value}</option>
          ))}
        </Select>
      </label>
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
      <Select
        aria-label="Instructions file"
        value={kind}
        onChange={(event) => setKind(event.target.value)}
      >
        {files.map((item) => (
          <option key={item.kind} value={item.kind}>
            {item.title}
            {item.exists ? "" : " · new"}
          </option>
        ))}
      </Select>
      <p className="quiet">{file?.path}</p>
      <textarea
        aria-label="Instructions"
        spellCheck={false}
        value={content}
        onChange={(event) => setContent(event.target.value)}
        placeholder="Write the guidance Medha should follow…"
      />
      <button
        className="btn-gold"
        disabled={busy || !file || content === file.content}
        onClick={() =>
          void onSave({ kind, content, before: file?.content }).catch(() => {})
        }
      >
        Save instructions
      </button>
    </div>
  );
}
