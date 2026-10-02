import { useEffect, useRef, useState } from "react";
import { Connectors } from "./Connectors";
import { Icon } from "./Icon";
import { useWorkspaceApi } from "./Workspace";
import { McpDetails, McpForm, mcpView as serverView, useMcpSignIn, type McpPrefill, type McpServer, type McpStatus } from "./McpServers";
import { McpCatalog } from "./McpCatalog";
import { PluginBrowse, PluginDoctor } from "./PluginBrowse";
import { PluginList, type Plugin } from "./PluginList";
import { SkillBrowse } from "./SkillBrowse";
import { Sheet } from "./Sheet";
import { SkillList, type Skill } from "./SkillList";
import { SkillManage } from "./SkillManage";
import { Add, Find, PageTop, summaryOf, ViewBar } from "./ExtensionParts";
import { Hooks } from "./Hooks";
import { ToolPreset } from "./ToolPreset";
type Mcp = McpServer;
type Update = {
  id: string;
  from_version: string;
  to_version: string;
  from_commit: string;
  to_commit: string;
  up_to_date: boolean;
  access_changed: boolean;
  blocked?: string;
};
type Tool = {
  name: string;
  description: string;
  category: string;
  radius: string;
};
// How far a tool reaches, in the words a person would use for it.
const REACH: Record<string, string> = {
  read: "Looks only",
  reversible_local: "Changes you can undo",
  irreversible_local: "Changes that stay",
  external: "Reaches outside",
};
export function Extensions({
  sessionKey,
  ensureOpen,
  locked,
  onAsk,
}: {
  sessionKey: string | null;
  ensureOpen: () => Promise<void>;
  locked: boolean;
  onAsk: (prompt: string) => void;
}) {
  const api = useWorkspaceApi();
  const [updating, setUpdating] = useState<Update>();
  const [removing, setRemoving] = useState<{
    title: string;
    method: string;
    params: Record<string, unknown>;
  }>();
  const [statuses, setStatuses] = useState<Record<string, McpStatus>>({});
  const [statusesRead, setStatusesRead] = useState(false);
  const [mcpOpen, setMcpOpen] = useState("");
  const [pluginView, setPluginView] = useState<"installed" | "browse">("installed");
  const [skillView, setSkillView] = useState<"installed" | "browse" | "manage">("installed");
  const [mcpView, setMcpView] = useState<"yours" | "catalog">("yours");
  const [prefill, setPrefill] = useState<McpPrefill>();
  const onConnectors = useRef(true);
  const signIn = useMcpSignIn(sessionKey, (event) => {
    setRefresh((value) => value + 1);
    // The Connectors sheet reports its own sign-ins.
    if (onConnectors.current) return;
    setNotice(event.ok ? `Signed in to ${event.server}` : "");
    if (!event.ok) setError(`Sign-in to ${event.server} failed: ${event.error ?? "unknown error"}`);
  });
  const [tab, setTab] = useState("connectors");
  onConnectors.current = tab === "connectors";
  const [query, setQuery] = useState("");
  const filter = useRef<HTMLInputElement>(null);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const typing = event.target instanceof HTMLElement && event.target.closest("input, textarea, [contenteditable]");
      if (event.key === "/" && !typing && filter.current) {
        event.preventDefault();
        filter.current.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  const [plugins, setPlugins] = useState<Plugin[]>([]);
  const [skills, setSkills] = useState<Skill[]>([]);
  const [mcp, setMcp] = useState<Mcp[]>([]);
  const [tools, setTools] = useState<Tool[]>([]);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const [source, setSource] = useState("");
  const [review, setReview] = useState<Plugin>();
  const [connect, setConnect] = useState<Mcp>();
  const [autoConnect, setAutoConnect] = useState(false);
  const [adding, setAdding] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [notices, setNotices] = useState<string[]>([]);
  useEffect(() => {
    let active = true;
    setError("");
    void Promise.all([api.extensions(), api.settings()])
      .then(([catalog, settings]) => {
        if (!active) return;
        setPlugins(catalog.plugins as Plugin[]);
        setSkills(catalog.skills as Skill[]);
        setNotices([
          ...(catalog.notices as string[]),
          ...((catalog.skill_errors as string[]) || []),
        ]);
        setMcp(settings.mcp as Mcp[]);
      })
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [refresh, sessionKey]);
  const showStatuses = (servers: (McpStatus & { server: string })[]) => {
    setStatuses(Object.fromEntries(servers.map((server) => [server.server, server])));
    setStatusesRead(true);
  };
  useEffect(() => {
    if (!sessionKey) return;
    let active = true;
    void ensureOpen()
      .then(() => api.liveCall(sessionKey, "extensions.catalog"))
      .then((catalog) => {
        if (active) {
          setTools((catalog.tools as Tool[]) ?? []);
          showStatuses((catalog.mcp as (McpStatus & { server: string })[]) || []);
          if (catalog.skills) setSkills(catalog.skills as Skill[]);
          const runtime = catalog.plugins as { plugins: Plugin[] };
          if (runtime?.plugins) setPlugins(runtime.plugins);
        }
      })
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [tab, sessionKey, refresh]);
  // The chat announces every connection change, so the page never has to ask again.
  useEffect(() => {
    if (!sessionKey) return;
    let off: (() => void) | undefined;
    let gone = false;
    void api
      .onLive((key, frame) => {
        if (key === sessionKey && frame.method === "mcp.status")
          showStatuses(((frame.params as { servers?: (McpStatus & { server: string })[] })?.servers) ?? []);
      })
      .then((unlisten) => (gone ? unlisten() : (off = unlisten)));
    return () => {
      gone = true;
      off?.();
    };
  }, [sessionKey]);
  async function act(method: string, params: Record<string, unknown>) {
    setBusy(true);
    setError("");
    setNotice("");
    try {
      const result = method.startsWith("settings.")
        ? await api.settings(method, params)
        : await api.extensions(method, params);
      setRefresh((value) => value + 1);
      setReview(undefined);
      setRemoving(undefined);
      setUpdating(undefined);
      setAdding(false);
      setInstalling(false);
      setSource("");
      setNotice(
        method === "extensions.install"
          ? "Installed and off. Open the plugin to review its access."
          : method === "extensions.skill.install"
            ? result.enabled
              ? "Skill installed"
              : `Installed and off. Review these findings before enabling: ${(result.findings as string[]).join("; ")}`
            : method === "settings.mcp.update"
              ? // The shared host applies a trusted remote server's change to every chat itself.
                mcp.some(
                  (s) =>
                    s.id === params.id &&
                    s.url &&
                    !s.executable &&
                    s.trust === "trusted",
                )
                ? "Saved"
                : "Saved. Reconnect the server to apply it to this chat."
              : method === "settings.mcp.signout"
                ? "Signed out. The server stays configured."
                : "Saved",
      );
      window.dispatchEvent(new CustomEvent("medha-preferences"));
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(false);
    }
  }
  const matches = (name: string, description = "") =>
    `${name} ${description}`.toLowerCase().includes(query.toLowerCase());
  return (
    <div className="app-page">
      <div className="page-content extensions-page">
        <div className="extension-head">
          <div className="page-tabs" role="tablist" aria-label="Extensions">
            {["connectors", "skills", "plugins", "MCP", "hooks", "tool access"].map((name) => (
              <button
                key={name}
                role="tab"
                aria-selected={tab === name}
                onClick={(event) => {
                  event.currentTarget.scrollIntoView({ block: "nearest", inline: "nearest" });
                  setTab(name);
                  setQuery("");
                }}
              >
                {name[0].toUpperCase() + name.slice(1)}
                {name === "plugins" &&
                  plugins.some((plugin) =>
                    plugin.activation.startsWith("changed"),
                  ) && <span className="need-dot" aria-label="Needs review" />}
              </button>
            ))}
          </div>
          {/* Browse views lead with their own search; one search box per view. */}
          {/* A redrawn tab carries its search in its own bar. */}
          {tab === "connectors" && (
              <input
                ref={filter}
                className="filter-input"
                aria-label={tab === "connectors" ? "Search apps" : "Filter extensions"}
                placeholder={tab === "connectors" ? "Search apps" : "Filter"}
                value={query}
                onChange={(event) => setQuery(event.target.value)}
              />
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
        {tab === "connectors" && (
          <Connectors
            sessionKey={sessionKey}
            ensureOpen={ensureOpen}
            statuses={statusesRead ? statuses : undefined}
            hashes={Object.fromEntries(mcp.map((server) => [server.id, server.hash]))}
            query={query}
            refresh={refresh}
            onChanged={() => setRefresh((value) => value + 1)}
            onAsk={onAsk}
          />
        )}
        {tab === "skills" && (
          <>
            <PageTop title="Skills">What Medha knows how to do. It loads one when your task calls for it.</PageTop>
            <ViewBar
              views={[
                { id: "installed", label: "Installed", count: skills.length },
                { id: "browse", label: "Browse" },
                { id: "manage", label: "Manage" },
              ]}
              view={skillView}
              onView={setSkillView}
            >
              {skillView === "installed" && (
                <>
                  <Find box={filter} label="Find a skill" value={query} onChange={setQuery} />
                  <Add label="Add skill" open={installing} onClick={() => setInstalling(!installing)} />
                </>
              )}
            </ViewBar>
          </>
        )}
        {tab === "skills" && skillView === "browse" && (
          <SkillBrowse
            installed={new Set(skills.map((skill) => skill.name))}
            busy={busy || locked}
            onInstall={(from) => act("extensions.skill.install", { source: from })}
          />
        )}
        {tab === "skills" && skillView === "manage" && (
          <SkillManage busy={busy || locked} onChanged={() => setRefresh((value) => value + 1)} />
        )}
        {tab === "skills" && skillView === "installed" && (
          <>
            {installing && (
              <form
                className="plugin-install"
                onSubmit={(event) => {
                  event.preventDefault();
                  void act("extensions.skill.install", { source });
                }}
              >
                <label>
                  <Icon name="plus" />
                  <input
                    required
                    autoFocus
                    aria-label="Skill source"
                    value={source}
                    onChange={(event) => setSource(event.target.value)}
                    placeholder="Skill folder, SKILL.md, or GitHub folder URL"
                  />
                </label>
                <button className="btn-line" disabled={busy || locked || !source.trim()}>
                  Install skill
                </button>
              </form>
            )}
            <SkillList
              skills={skills}
              matches={matches}
              searching={Boolean(query)}
              disabled={busy || locked}
              onToggle={(skill) => void act("extensions.skill.configure", { name: skill.name, enabled: !skill.enabled })}
              onRemove={(skill) =>
                setRemoving({
                  title: `Remove ${skill.name}?`,
                  method: "extensions.skill.remove",
                  params: { name: skill.name },
                })
              }
              onAsk={onAsk}
            />
          </>
        )}
        {tab === "plugins" && (
          <>
            <PageTop title="Plugins">
              Bundles of skills, servers and scripts. One stays off until you have seen what it can touch.
            </PageTop>
            <ViewBar
              views={[
                { id: "installed", label: "Installed", count: plugins.length },
                { id: "browse", label: "Browse" },
              ]}
              view={pluginView}
              onView={setPluginView}
            >
              {pluginView === "installed" && (
                <>
                  <Find box={filter} label="Find a plugin" value={query} onChange={setQuery} />
                  <Add label="Add plugin" open={installing} onClick={() => setInstalling(!installing)} />
                </>
              )}
            </ViewBar>
          </>
        )}
        {tab === "plugins" && pluginView === "browse" && (
          <PluginBrowse
            busy={busy || locked}
            onInstall={(from) => act("extensions.install", { source: from })}
          />
        )}
        {tab === "plugins" && pluginView === "installed" && (
          <>
            {installing && (
              <form
                className="plugin-install"
                onSubmit={(event) => {
                  event.preventDefault();
                  void act("extensions.install", { source });
                }}
              >
                <label>
                  <Icon name="plus" />
                  <input
                    required
                    autoFocus
                    aria-label="Plugin source"
                    value={source}
                    onChange={(event) => setSource(event.target.value)}
                    placeholder="owner/repo, name@marketplace, or a folder"
                  />
                </label>
                <button className="btn-line" disabled={busy || locked || !source.trim()}>
                  Install
                </button>
              </form>
            )}
            {notices.map((message) => (
              <p className="surface-error" key={message}>
                {message}
              </p>
            ))}
            <PluginList
              plugins={plugins.filter((plugin) => matches(plugin.name, plugin.description))}
              disabled={busy || locked}
              onReview={setReview}
              onTurnOff={(plugin) => void act("extensions.disable", { id: plugin.id, scope: plugin.scope })}
              onCheckUpdate={(plugin) => {
                setBusy(true);
                setError("");
                void api
                  .extensions("extensions.update.preview", { id: plugin.id })
                  .then((result) => {
                    const plan = result as Update;
                    if (plan.up_to_date) setNotice("Already up to date");
                    else setUpdating(plan);
                  })
                  .catch((cause) => setError(String(cause)))
                  .finally(() => setBusy(false));
              }}
              onRollback={(plugin) =>
                setRemoving({
                  title: `Restore the previous version of ${plugin.name}?`,
                  method: "extensions.rollback",
                  params: { id: plugin.id },
                })
              }
              onRemove={(plugin) =>
                setRemoving({
                  title: `Remove ${plugin.name}?`,
                  method: "extensions.remove",
                  params: { id: plugin.id },
                })
              }
            />
            <PluginDoctor />
          </>
        )}
        {tab === "MCP" && (
          <>
            <PageTop title="Servers">
              Every tool server Medha can reach, with its sign-in, tools and access. Medha asks before starting one you
              have not trusted.
            </PageTop>
            <ViewBar
              views={[
                { id: "yours", label: "Yours", count: mcp.length },
                { id: "catalog", label: "Catalog" },
              ]}
              view={mcpView}
              onView={setMcpView}
            >
              {mcpView === "yours" && <Find box={filter} label="Find a server" value={query} onChange={setQuery} />}
              <Add
                label="Add server"
                open={adding && mcpView === "yours"}
                onClick={() => {
                  setPrefill(undefined);
                  setMcpView("yours");
                  setAdding(!adding);
                }}
              />
            </ViewBar>
            {mcpView === "catalog" && (
              <McpCatalog
                onPick={(picked) => {
                  setPrefill({ ...picked });
                  setAdding(true);
                  setMcpView("yours");
                }}
              />
            )}
            {mcpView === "yours" && adding && (
              <McpForm
                key={prefill ? `${prefill.id}:${prefill.url ?? prefill.command?.join(" ")}` : "blank"}
                initial={prefill}
                busy={busy}
                onSave={(args) => act("settings.mcp.save", { args })}
              />
            )}
            {mcpView === "yours" && <>
            <div className="ix">
            {mcp
              .filter((server) => matches(server.id, server.url))
              .map((server) => {
                const view = serverView(server, statuses[server.id], signIn.pending?.server === server.id);
                const signInTo =
                  server.url && sessionKey
                    ? () => {
                        setBusy(true);
                        setError("");
                        signIn.start(server.id);
                        void ensureOpen()
                          .then(() => api.liveCall(sessionKey, "mcp.signin", { id: server.id, hash: server.hash }))
                          .then(() => setNotice(`Finish signing in to ${server.id} in your browser.`))
                          .catch((cause) => setError(String(cause)))
                          .finally(() => setBusy(false));
                      }
                    : undefined;
                const state = (
                  <small className="mcp-state">
                    <span className={`agent-dot ${view.dot}`} aria-hidden="true" />
                    {view.label}
                  </small>
                );
                // The one thing the server needs now; the same button on the row and in the sheet.
                const action = (
                  <>
                    {view.action === "connect" && (
                      <button
                        className="btn-line"
                        disabled={busy || locked || !sessionKey}
                        title={sessionKey ? undefined : "Open a chat to connect"}
                        onClick={() => {
                          // The question that follows is asked on the page, not over the sheet.
                          setMcpOpen("");
                          setAutoConnect(server.trust === "trusted");
                          setConnect(server);
                        }}
                      >
                        Connect
                      </button>
                    )}
                    {view.action === "signin" && signInTo && (
                      <button className="btn-line" disabled={busy || locked} onClick={signInTo}>
                        Sign in
                      </button>
                    )}
                    {view.action === "disconnect" && (
                      <button
                        className="btn-line"
                        disabled={busy || locked}
                        onClick={() => {
                          setBusy(true);
                          void api
                            .liveCall(sessionKey!, "mcp.disconnect", {
                              id: server.id,
                            })
                            .then(() => setRefresh((value) => value + 1))
                            .catch((cause) => setError(String(cause)))
                            .finally(() => setBusy(false));
                        }}
                      >
                        Disconnect
                      </button>
                    )}
                  </>
                );
                return (
                  <div className="ix-row" key={server.id}>
                    <button className="ix-main" onClick={() => setMcpOpen(server.id)}>
                      <b>{server.id}</b>
                      <span className="ix-sum mono">{(server.url || server.executable || "").replace(/^https:\/\//, "")}</span>
                    </button>
                    {state}
                    <span className="ix-act">{action}</span>
                    {mcpOpen === server.id && (
                      <Sheet label={server.id} onClose={() => setMcpOpen("")}>
                        <div className="sheet-title">
                          <h2>{server.id}</h2>
                        </div>
                        <p className="sheet-from mono">{server.url || server.executable}</p>
                        {state}
                        <McpDetails
                          server={server}
                          status={statuses[server.id]}
                          tools={tools}
                          busy={busy || locked}
                          onUpdate={(patch) => void act("settings.mcp.update", { id: server.id, ...patch })}
                          onSignIn={signInTo}
                          onSignOut={() => void act("settings.mcp.signout", { id: server.id })}
                        />
                        <div className="sheet-actions">
                          {action}
                          <button
                            className="sheet-quiet"
                            disabled={busy || locked}
                            onClick={() => {
                              setMcpOpen("");
                              setRemoving({
                                title: `Remove MCP server ${server.id}?`,
                                method: "settings.mcp.remove",
                                params: { id: server.id },
                              });
                            }}
                          >
                            Remove server
                          </button>
                        </div>
                      </Sheet>
                    )}
                  </div>
                );
              })}
            </div>
            {signIn.pending && (
              <div className="mcp-signin" role="status">
                <span>Signing in to {signIn.pending.server} — finish in your browser.</span>
                {signIn.pending.url && (
                  <button className="btn-line" onClick={() => void api.openLink(signIn.pending!.url!)}>
                    Open the sign-in page
                  </button>
                )}
              </div>
            )}
            {!mcp.length && !adding && (
              <p className="quiet">
                No user MCP servers configured. Plugin MCP servers appear in
                Plugins.
              </p>
            )}
            </>}
          </>
        )}
        {tab === "hooks" && <Hooks locked={locked} />}
        {tab === "tool access" && (
          <>
            <PageTop title="Tool access">
              What Medha may reach for in new chats of this project. Chat mode controls approvals; every use is still
              checked by Medha’s policy, and asked about when it matters.
            </PageTop>
            <ToolPreset available={tools.length} locked={locked} />
            {tools.length > 0 && (
              <section className="ix-group">
                <ViewBar>
                  <Find box={filter} label="Find a tool" value={query} onChange={setQuery} />
                </ViewBar>
                <h3>
                  In this chat <span>{tools.length}</span>
                </h3>
                <div className="ix">
                  {tools
                    .filter((tool) => matches(tool.name, tool.description))
                    .map((tool) => (
                      <details className="ix-row tool" key={tool.name}>
                        <summary>
                          <span className="ix-main">
                            <b className="mono">{tool.name}</b>
                            <span className="ix-sum">{summaryOf(tool.description)}</span>
                          </span>
                          <small className={`ix-note reach-${tool.radius}`}>{REACH[tool.radius] ?? tool.radius.replaceAll("_", " ")}</small>
                        </summary>
                        <p>{tool.description}</p>
                      </details>
                    ))}
                </div>
              </section>
            )}
            {!sessionKey && (
              <p className="quiet">
                Open a chat to inspect its available tools.
              </p>
            )}
          </>
        )}
        {locked && (
          <p className="quiet">
            Finish or stop active work before changing extensions.
          </p>
        )}
        {connect && (
          <div className="dialog-backdrop">
            <section
              className="confirm-dialog"
              role="alertdialog"
              aria-modal="true"
              aria-labelledby="connect-title"
            >
              <h2 id="connect-title">Connect {connect.id}?</h2>
              <p>
                {connect.url
                  ? `Connect to ${connect.url}`
                  : "Start this local process:"}
              </p>
              {!connect.url && (
                <pre className="io">{connect.command.join(" ")}</pre>
              )}
              <p>Its tools become available in this chat.</p>
              <label className="checkbox-label">
                <input
                  type="checkbox"
                  checked={autoConnect}
                  onChange={(event) => setAutoConnect(event.target.checked)}
                />
                Connect automatically in new chats
              </label>
              <div className="row-actions">
                <button
                  className="btn-line"
                  onClick={() => setConnect(undefined)}
                >
                  Cancel
                </button>
                <button
                  className="btn-gold"
                  disabled={busy || locked}
                  onClick={() => {
                    setBusy(true);
                    setError("");
                    void ensureOpen()
                      .then(() =>
                        api.liveCall(sessionKey!, "mcp.connect", {
                          id: connect.id,
                          hash: connect.hash,
                        }),
                      )
                      .then(async (result) => {
                        setConnect(undefined);
                        // After connecting: saving changes the definition the connect was checked against.
                        if (autoConnect !== (connect.trust === "trusted")) {
                          await api.settings("settings.mcp.update", {
                            id: connect.id,
                            trust: autoConnect ? "trusted" : "workspace",
                          });
                        }
                        if (result.state === "signing_in") signIn.start(connect.id);
                        setNotice(
                          result.state === "ready"
                            ? `Connected · ${result.tools} tools`
                            : result.state === "signing_in"
                              ? `Finish signing in to ${connect.id} in your browser.`
                              : result.state === "needs_token"
                                ? `${connect.id} needs an API key or token. Remove it and add it again with one.`
                                : `${String(result.state).replaceAll("_", " ")}${result.detail ? `: ${result.detail}` : ""}`,
                        );
                        setRefresh((value) => value + 1);
                      })
                      .catch((cause) => setError(String(cause)))
                      .finally(() => setBusy(false));
                  }}
                >
                  Connect
                </button>
              </div>
              {error && (
                <p className="surface-error" role="alert">
                  {error}
                </p>
              )}
            </section>
          </div>
        )}
        {removing && (
          <div className="dialog-backdrop">
            <section
              className="confirm-dialog"
              role="alertdialog"
              aria-modal="true"
            >
              <h2>{removing.title}</h2>
              <p>This changes the saved extension configuration.</p>
              <div className="row-actions">
                <button
                  className="btn-line"
                  onClick={() => setRemoving(undefined)}
                >
                  Cancel
                </button>
                <button
                  className="btn-gold"
                  disabled={busy || locked}
                  onClick={() => void act(removing.method, removing.params)}
                >
                  Continue
                </button>
              </div>
            </section>
          </div>
        )}
        {updating && (
          <div className="dialog-backdrop">
            <section className="confirm-dialog" role="dialog" aria-modal="true">
              <h2>Update {updating.id}</h2>
              <p>
                {updating.from_version} → {updating.to_version}
              </p>
              <p>
                {updating.access_changed
                  ? "Its access has changed. The updated plugin stays off until you review and enable it."
                  : "Its requested access is unchanged."}
              </p>
              {updating.blocked && (
                <p className="surface-error">{updating.blocked}</p>
              )}
              <div className="row-actions">
                <button
                  className="btn-line"
                  onClick={() => setUpdating(undefined)}
                >
                  Cancel
                </button>
                <button
                  className="btn-gold"
                  disabled={busy || locked}
                  onClick={() =>
                    void act("extensions.update.apply", {
                      id: updating.id,
                      from_commit: updating.from_commit,
                      to_commit: updating.to_commit,
                    })
                  }
                >
                  Install update
                </button>
              </div>
            </section>
          </div>
        )}
        {review && (
          <div className="dialog-backdrop">
            <section
              className="confirm-dialog"
              role="alertdialog"
              aria-modal="true"
              aria-labelledby="enable-title"
            >
              <h2 id="enable-title">Enable {review.name}?</h2>
              <Access plugin={review} />
              <div className="row-actions">
                <button
                  className="btn-line"
                  onClick={() => setReview(undefined)}
                >
                  Keep off
                </button>
                <button
                  className="btn-gold"
                  disabled={busy || locked}
                  onClick={() =>
                    void act("extensions.enable", {
                      id: review.id,
                      scope: review.scope,
                      hash: review.hash,
                      grant: review.grant,
                    })
                  }
                >
                  Approve access and enable
                </button>
              </div>
            </section>
          </div>
        )}
      </div>
    </div>
  );
}
function Access({ plugin }: { plugin: Plugin }) {
  const items = [
    ...plugin.permissions.network_hosts.map((host) => `Connect to ${host}`),
    ...plugin.permissions.read_paths.map(
      (path) => `Read ${path || "workspace"}`,
    ),
    ...plugin.permissions.write_paths.map(
      (path) => `Write ${path || "workspace"}`,
    ),
    ...plugin.permissions.secrets.map((name) => `Secret handle: ${name}`),
  ];
  return items.length ? (
    <ul className="access-list">
      {items.map((item) => (
        <li key={item}>{item}</li>
      ))}
    </ul>
  ) : (
    <p className="quiet">No additional access requested.</p>
  );
}
