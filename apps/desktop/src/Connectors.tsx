import { useEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { Icon } from "./Icon";
import { useWorkspaceApi } from "./Workspace";
import { useMcpSignIn, type McpStatus } from "./McpServers";

export type Connector = {
  id: string;
  name: string;
  category: string;
  description: string;
  source: string;
  asks: string[];
  server: string | null;
  off?: boolean;
  activity: { last_used: number; this_week: number } | null;
  evidence: string | null;
};

type Phase = "idle" | "opening" | "waiting" | "done";

const CATEGORIES: [string, string][] = [
  ["productivity", "Productivity"],
  ["communication", "Communication"],
  ["design", "Design"],
  ["developer", "Developer"],
  ["analytics", "Analytics"],
  ["finance", "Finance"],
  ["sales", "Sales"],
  ["health", "Health"],
  ["search", "Search"],
];
const LABEL = Object.fromEntries(CATEGORIES);
const LOGOS = import.meta.glob("./assets/connectors/*.png", { eager: true, import: "default" }) as Record<string, string>;
const logo = (id: string) => LOGOS[`./assets/connectors/${id}.png`];
const domain = (c: Connector) => new URL(c.source).hostname.split(".").slice(-2).join(".");

function ago(ts: number) {
  const seconds = Date.now() / 1000 - ts;
  const count = (n: number, unit: string) => `${n} ${unit}${n === 1 ? "" : "s"} ago`;
  if (seconds < 90) return "just now";
  if (seconds < 3600) return count(Math.round(seconds / 60), "minute");
  if (seconds < 86400) return count(Math.round(seconds / 3600), "hour");
  if (seconds < 172800) return "yesterday";
  return count(Math.round(seconds / 86400), "day");
}

function usage(c: Connector) {
  if (!c.activity) return "Not used yet.";
  const week = c.activity.this_week;
  return `Used ${ago(c.activity.last_used)}.${week ? ` ${week} ${week === 1 ? "action" : "actions"} this week, all approved by you.` : ""}`;
}

export function Connectors({
  sessionKey,
  ensureOpen,
  statuses,
  hashes,
  query,
  refresh,
  onChanged,
  onAsk,
}: {
  sessionKey: string | null;
  ensureOpen: () => Promise<void>;
  /** Undefined until the chat's bridge has reported once. */
  statuses: Record<string, McpStatus> | undefined;
  hashes: Record<string, string>;
  query: string;
  refresh: number;
  onChanged: () => void;
  onAsk: (prompt: string) => void;
}) {
  const api = useWorkspaceApi();
  const [all, setAll] = useState<Connector[]>([]);
  const [error, setError] = useState("");
  const [openId, setOpenId] = useState<string>();
  const [leaving, setLeaving] = useState(false);
  const [phase, setPhase] = useState<Phase>("idle");
  const [problem, setProblem] = useState("");
  const created = useRef<string | null>(null);
  const waitingFor = useRef<string | null>(null);
  const signIn = useMcpSignIn(sessionKey, (event) => {
    if (event.server !== waitingFor.current) return;
    waitingFor.current = null;
    if (event.ok) {
      setPhase("done");
      onChanged();
    } else {
      setPhase("idle");
      setProblem(`Sign-in didn't finish: ${event.error ?? "the browser was closed"}`);
    }
  });

  useEffect(() => {
    let active = true;
    void api
      .extensions("extensions.connectors")
      .then((result) => active && setAll(result.connectors as Connector[]))
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [refresh]);

  const open = all.find((c) => c.id === openId);
  const state = (c: Connector) => (c.server ? statuses?.[c.server]?.state : undefined);
  const needle = query.trim().toLowerCase();
  const shown = all.filter((c) => !needle || [c.name, c.description, ...c.asks].join(" ").toLowerCase().includes(needle));
  const on = (c: Connector) => Boolean(c.server) && !c.off;
  const mine = shown.filter(on);
  const suggested = shown.filter((c) => !on(c) && c.evidence);
  const rest = shown.filter((c) => !on(c) && !c.evidence);

  function show(id: string | undefined) {
    setLeaving(false);
    setOpenId(id);
    setPhase(id && all.find((c) => c.id === id && state(c) === "ready") ? "done" : "idle");
    setProblem("");
  }

  // The real sheet animates out, so glass stays glass until it is gone.
  function close() {
    if (!openId) return;
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return show(undefined);
    setLeaving(true);
  }

  useEffect(() => {
    if (!leaving) return;
    const done = setTimeout(() => show(undefined), 600);
    return () => clearTimeout(done);
  }, [leaving]);

  useEffect(() => {
    if (!openId) return;
    const onKey = (event: KeyboardEvent) => event.key === "Escape" && close();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  async function connect(c: Connector) {
    if (!sessionKey) return setProblem("Open a chat in this workspace first, then connect.");
    setProblem("");
    setPhase("opening");
    try {
      await ensureOpen();
      const expired = c.server && state(c) === "needs_auth";
      const result = expired
        ? await api.liveCall(sessionKey, "mcp.signin", { id: c.server, hash: hashes[c.server!] })
        : await api.liveCall(sessionKey, "connectors.connect", { id: c.id });
      const server = (result.server as string) ?? c.server;
      if (!c.server) created.current = server;
      if (result.state === "signing_in") {
        waitingFor.current = server;
        signIn.start(server);
        setPhase("waiting");
      } else if (result.state === "ready") {
        setPhase("done");
        onChanged();
      } else {
        setPhase("idle");
        setProblem(result.detail ? `Couldn't connect: ${String(result.detail)}` : "Couldn't connect. Try again in a moment.");
      }
    } catch (cause) {
      setPhase("idle");
      setProblem(String(cause));
    }
  }

  async function disconnect(server: string, forget: boolean) {
    waitingFor.current = null;
    const live = sessionKey && (await api.liveCall(sessionKey, "mcp.disconnect", { id: server }).then(() => true, () => false));
    // No chat is running to switch it off: the saved setting does, for every chat.
    if (!live && !forget) await api.settings("settings.mcp.update", { id: server, disabled: true }).catch(() => undefined);
    if (forget) await api.settings("settings.mcp.remove", { id: server }).catch(() => undefined);
    onChanged();
  }

  async function cancel() {
    const server = waitingFor.current ?? created.current;
    setPhase("idle");
    if (server) await disconnect(server, server === created.current);
    created.current = null;
  }

  const row = (c: Connector, detail: string, trailing: ReactNode, kind = "") => (
    <button
      key={c.id}
      className={`cx-row ${kind}`}
      onClick={() => show(c.id)}
      aria-label={`${c.name}: ${detail}`}
    >
      <img className="cx-plate" src={logo(c.id)} alt="" />
      <span className="cx-text">
        <span className="cx-name">{c.name}</span>
        <span className="cx-desc">{detail}</span>
      </span>
      {trailing}
    </button>
  );

  const chevron = (
    <span className="cx-go" aria-hidden="true">
      <Icon name="chev" />
    </span>
  );

  function connectedRow(c: Connector) {
    const s = state(c);
    const tools = c.server ? statuses?.[c.server]?.tools : undefined;
    if (s === "needs_auth")
      return row(c, "Sign-in expired. The app asked for a fresh sign-in.", <span className="cx-fix">Sign in again</span>, "cx-ledger expired");
    if (s === "failed" || s === "parked")
      return row(c, "Couldn't reach it. Open to try again.", <span className="cx-fix">Retry</span>, "cx-ledger expired");
    const trailing =
      s === "ready" ? (
        <span className="cx-state">
          <span className="dot" />
          {tools} {tools === 1 ? "tool" : "tools"}
        </span>
      ) : (
        <span className="cx-state dim">
          {!statuses ? "" : s === "connecting" || s === "reconnecting" ? "Connecting…" : "Connects in new chats"}
        </span>
      );
    return row(c, usage(c), trailing, "cx-ledger");
  }

  return (
    <div className="cx-page">
      <h2>Connectors</h2>
      <p className="cx-lede">Sign in once and Medha can work in the apps you already use. Your sign-ins stay on this computer.</p>
      {error && (
        <p className="surface-error" role="alert">
          {error}
        </p>
      )}
      {mine.length > 0 && (
        <section className="cx-section">
          <h3>
            Connected <span>{mine.length}</span>
          </h3>
          <div className="cx-rows one">{mine.map(connectedRow)}</div>
        </section>
      )}
      {suggested.length > 0 && (
        <section className="cx-section">
          <h3>Suggested for this project</h3>
          <div className="cx-rows">{suggested.map((c) => row(c, c.evidence!, chevron, "cx-suggested"))}</div>
        </section>
      )}
      {CATEGORIES.map(([key, label]) => {
        const items = rest.filter((c) => c.category === key);
        return (
          items.length > 0 && (
            <section className="cx-section" key={key}>
              <h3>
                {label} <span>{items.length}</span>
              </h3>
              <div className="cx-rows">{items.map((c) => row(c, c.description, chevron))}</div>
            </section>
          )
        );
      })}
      {all.length > 0 && !shown.length && <p className="quiet">No apps match “{query.trim()}”.</p>}
      {open &&
        createPortal(
        <>
          <div className={`cx-scrim${leaving ? " leaving" : ""}`} onClick={close} />
          <aside
            className={`cx-sheet${leaving ? " leaving" : ""}`}
            role="dialog"
            aria-modal="true"
            aria-label={open.name}
            onAnimationEnd={(event) => {
              if (leaving && event.target === event.currentTarget) show(undefined);
            }}
          >
            <button className="cx-close" aria-label="Close" autoFocus onClick={close}>
              <Icon name="x" />
            </button>
            <div className="cx-hero">
              <img className="cx-plate" src={logo(open.id)} alt="" />
              <div>
                <h2>{open.name}</h2>
                <small>{LABEL[open.category]}</small>
              </div>
            </div>
            <p className="cx-about">
              {open.description} Ask Medha in plain words and it works in {open.name} for you.
            </p>
            <h4>Try asking</h4>
            <ul className="cx-asks">
              {open.asks.map((ask) => (
                <li key={ask}>
                  <button
                    disabled={phase !== "done"}
                    title={phase === "done" ? "Start a chat with this" : `Connect ${open.name} first`}
                    onClick={() => onAsk(ask)}
                  >
                    {ask}
                  </button>
                </li>
              ))}
            </ul>
            <ul className="cx-facts">
              <li>
                <Icon name="lock" />
                <span>You sign in on {open.name}'s own page. Medha never sees your password.</span>
              </li>
              <li>
                <Icon name="shield" />
                <span>Medha asks you before every action it takes in {open.name}.</span>
              </li>
              <li>
                <Icon name="link" />
                <span>Provided by {open.name}. Disconnect any time.</span>
              </li>
            </ul>
            <Action
              connector={open}
              phase={phase}
              expired={state(open) === "needs_auth"}
              link={signIn.pending?.server === waitingFor.current ? signIn.pending?.url : undefined}
              onConnect={() => void connect(open)}
              onCancel={() => void cancel()}
              onDisconnect={() => {
                const server = open.server ?? created.current;
                if (server) void disconnect(server, true);
                created.current = null;
                setPhase("idle");
              }}
              onOpenLink={(url) => void api.openLink(url)}
            />
            {problem && (
              <p className="cx-problem" role="alert">
                {problem}
              </p>
            )}
          </aside>
        </>,
          document.body,
        )}
    </div>
  );
}

/** The sheet's one action: the button becomes the progress, then the result. */
function Action({
  connector,
  phase,
  expired,
  link,
  onConnect,
  onCancel,
  onDisconnect,
  onOpenLink,
}: {
  connector: Connector;
  phase: Phase;
  expired: boolean;
  link?: string;
  onConnect: () => void;
  onCancel: () => void;
  onDisconnect: () => void;
  onOpenLink: (url: string) => void;
}) {
  const label = {
    idle: expired ? "Sign in again" : `Connect ${connector.name}`,
    opening: `Opening ${connector.name}`,
    waiting: `Waiting for ${connector.name}`,
    done: "Connected",
  }[phase];
  return (
    <div className="cx-foot">
      <button
        className={`cx-connect ${phase === "idle" ? "gild" : phase === "done" ? "done" : "busy"} ${phase === "waiting" ? "waiting" : ""}`}
        onClick={phase === "idle" ? onConnect : undefined}
        aria-live="polite"
      >
        <span key={phase} className="cx-label">
          {phase === "done" && <Icon name="check" />}
          {label}
          {phase === "idle" && <Icon name="external" className="icon ext" />}
        </span>
      </button>
      <p className="cx-hint">
        {phase === "idle" && `You'll sign in on ${domain(connector)}`}
        {phase === "opening" && "A browser tab is on its way"}
        {phase === "waiting" && (
          <>
            Come back here when you're done.{" "}
            {link && (
              <button className="cx-link" onClick={() => onOpenLink(link)}>
                Open the page again
              </button>
            )}{" "}
            <button className="cx-link quiet" onClick={onCancel}>
              Cancel
            </button>
          </>
        )}
        {phase === "done" && (
          <>
            Ready to use.{" "}
            <button className="cx-link" onClick={onDisconnect}>
              Disconnect
            </button>
          </>
        )}
      </p>
    </div>
  );
}
