import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Icon } from "./Icon";
import { useWorkspaceApi } from "./Workspace";
import {
  authFor,
  contextLabel,
  hostOf,
  isLocal,
  matching,
  plainName,
  profileFor,
  reasonFor,
  sizeNote,
  type Found,
  type Protocol,
  type Provider,
} from "./modelSetup";

type Step = "choose" | "connect" | "models" | "manual";
type Choice = { name: string; url: string; auth: string; custom: boolean };

/**
 * The first thing a new person sees: connect one model, in as few steps as the
 * choice allows. A server on this computer needs none beyond picking its model.
 */
export function FirstRun({ onDone, onClose }: { onDone: () => void; onClose: () => void }) {
  const api = useWorkspaceApi();
  const [protocols, setProtocols] = useState<Protocol[]>([]);
  const [protocol, setProtocol] = useState("");
  const [running, setRunning] = useState<Record<string, Found[]>>({});
  const [step, setStep] = useState<Step>("choose");
  const [choice, setChoice] = useState<Choice>();
  const [url, setUrl] = useState("");
  const [key, setKey] = useState("");
  const [found, setFound] = useState<Found[]>([]);
  const [picked, setPicked] = useState("");
  const [query, setQuery] = useState("");
  const [typed, setTyped] = useState("");
  const [context, setContext] = useState("");
  const [published, setPublished] = useState<{ model: string; size: number | null }>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const attempt = useRef(0);

  const active = protocols.find((item) => item.value === protocol);
  const connection = { protocol, url, auth: choice ? authFor(choice, key) : "none" };

  useEffect(() => {
    let current = true;
    void api
      .settings()
      .then((result) => {
        if (!current) return;
        const list = (result.protocols as Protocol[]).filter((item) => item.available);
        setProtocols(list);
        setProtocol(list[0]?.value ?? "");
        // Local servers are looked for unasked, so one that is running needs no setup.
        for (const item of list)
          for (const provider of item.providers.filter((entry) => isLocal(entry.url)))
            void api
              .settings("settings.model.discover", {
                profile: profileFor({ protocol: item.value, url: provider.url, auth: "none" }),
                key: "",
              })
              .then((reply) => {
                const models = reply.models as Found[];
                if (current && models.length) setRunning((all) => ({ ...all, [provider.url]: models }));
              })
              .catch(() => {});
      })
      .catch((cause) => current && setError(String(cause)));
    return () => {
      current = false;
    };
  }, [api]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => event.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const shown = useMemo(() => matching(found, query), [found, query]);

  function back() {
    attempt.current++;
    setBusy(false);
    setError("");
    setStep("choose");
  }

  function list(models: Found[]) {
    setFound(models);
    setPicked(models[0]?.id ?? "");
    setQuery("");
    setContext("");
    setStep(models.length ? "models" : "manual");
  }

  async function discover(next = connection, secret = key) {
    const mine = ++attempt.current;
    setBusy(true);
    setError("");
    try {
      const reply = await api.settings("settings.model.discover", { profile: profileFor(next), key: secret });
      if (mine === attempt.current) list(reply.models as Found[]);
    } catch (cause) {
      if (mine === attempt.current) setError(reasonFor(cause, next.url));
    } finally {
      if (mine === attempt.current) setBusy(false);
    }
  }

  function choose(provider: Provider | null) {
    const next: Choice = provider
      ? { name: plainName(provider.name), url: provider.url, auth: provider.auth, custom: false }
      : { name: "Custom endpoint", url: "", auth: active?.auth ?? "bearer", custom: true };
    setChoice(next);
    setUrl(next.url);
    setKey("");
    setError("");
    const ready = running[next.url];
    if (ready) return list(ready);
    setStep("connect");
    if (!next.custom && next.auth === "none") void discover({ protocol, url: next.url, auth: "none" }, "");
  }

  async function save(model: string, size: number | null) {
    setBusy(true);
    setError("");
    try {
      await api.settings("settings.model.save", {
        name: "",
        profile: profileFor(connection, model, size),
        key,
        default: true,
      });
      window.dispatchEvent(new CustomEvent("medha-preferences"));
      onDone();
    } catch (cause) {
      setError(String(cause).replace(/^Error:\s*/, ""));
      setBusy(false);
    }
  }

  const chosen = found.find((model) => model.id === picked);
  const size = Number(context) > 0 ? Number(context) : null;
  const start = () => (step === "manual" ? void save(typed.trim(), size) : void save(picked, chosen?.context_length ?? size));

  // The model whose size nobody gave: typed by hand, or listed without one.
  const unsized = step === "manual" ? typed.trim() : step === "models" && chosen && !chosen.context_length ? chosen.id : "";
  useEffect(() => {
    if (!unsized) return;
    let current = true;
    const timer = window.setTimeout(() => {
      void api
        .settings("settings.model.context", { model: unsized })
        .then((reply) => current && setPublished({ model: unsized, size: (reply.published as number | null) ?? null }))
        .catch(() => {});
    }, 300);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [api, unsized]);

  const sizeField = (
    <>
      <label>
        Context size, in tokens
        <input className="fr-in" inputMode="numeric" value={context} placeholder="Optional, for example 128000" onChange={(event) => setContext(event.target.value.replace(/\D/g, ""))} />
      </label>
      <p className="fr-note">{sizeNote(step === "models", published?.model === unsized ? published.size : undefined)}</p>
    </>
  );

  const title = { choose: "Connect a model", connect: choice?.custom ? "Your own server" : "Connect", models: "Choose a model", manual: "Name the model" }[step];
  const lead =
    step === "choose"
      ? "Medha needs one model to start. Pick where yours runs."
      : step === "models"
        ? isLocal(url)
          ? `${choice?.name} is running on this computer. Nothing leaves it.`
          : `${found.length} ${found.length === 1 ? "model" : "models"} on ${choice?.name}.`
        : step === "manual"
          ? "The server did not list its models. Type the name it uses."
          : choice?.custom
            ? `Any server that speaks ${active?.label ?? "this protocol"}.`
            : choice?.auth === "none"
              ? `Looking for ${choice.name} on this computer.`
              : `Your key stays on this computer and is sent only to ${choice?.name}.`;

  const local = active?.providers.filter((provider) => isLocal(provider.url)) ?? [];
  const hosted = active?.providers.filter((provider) => !isLocal(provider.url)) ?? [];
  const row = (provider: Provider) => {
    const models = running[provider.url];
    return (
      <li key={provider.url}>
        <button type="button" className="fr-row" onClick={() => choose(provider)}>
          <i className={models ? "live" : undefined} />
          {plainName(provider.name)}
          <small className={models ? "live" : undefined}>
            {models
              ? `Running, ${models.length} ${models.length === 1 ? "model" : "models"}`
              : provider.auth === "none"
                ? hostOf(provider.url)
                : "Needs a key"}
          </small>
          <Icon name="chev" />
        </button>
      </li>
    );
  };

  return createPortal(
    <div className="fr-scrim">
      <form
        className="fr-sheet"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onSubmit={(event) => {
          event.preventDefault();
          if (busy) return;
          if (step === "connect") void discover();
          else if (step !== "choose") start();
        }}
      >
        <header className="fr-head">
          <h2>{title}</h2>
          <p>{lead}</p>
        </header>

        {step === "choose" && (
          <>
            {protocols.length > 1 && (
              <div className="fr-seg" role="group" aria-label="Protocol">
                {protocols.map((item) => (
                  <button type="button" key={item.value} aria-pressed={item.value === protocol} onClick={() => setProtocol(item.value)}>
                    {item.label}
                  </button>
                ))}
              </div>
            )}
            <ul className="fr-list">
              {local.length > 0 && <li className="fr-group">On this computer</li>}
              {local.map(row)}
              {hosted.length > 0 && <li className="fr-group">Providers</li>}
              {hosted.map(row)}
              <li className="fr-group">Your own</li>
              <li>
                <button type="button" className="fr-row" onClick={() => choose(null)}>
                  <i />
                  Custom endpoint
                  <small>Any {active?.label ?? ""} server</small>
                  <Icon name="chev" />
                </button>
              </li>
            </ul>
          </>
        )}

        {step !== "choose" && choice && (
          <div className="fr-picked">
            {choice.name}
            <small>{url ? hostOf(url) : active?.label}</small>
            <button type="button" className="fr-link" onClick={back}>
              Change
            </button>
          </div>
        )}

        {step === "connect" && choice && (
          <div className="fr-body">
            {(choice.custom || choice.auth === "none") && (
              <label>
                Address
                <input className="fr-in" required type="url" autoFocus={choice.custom} value={url} placeholder="http://localhost:8000/v1" onChange={(event) => setUrl(event.target.value)} />
              </label>
            )}
            {(choice.custom || choice.auth !== "none") && (
              <label>
                {choice.custom ? "API key, if it needs one" : "API key"}
                <input
                  className="fr-in"
                  type="password"
                  autoComplete="new-password"
                  autoFocus={!choice.custom}
                  required={!choice.custom}
                  value={key}
                  placeholder={choice.custom ? "Leave empty for a local server" : "Paste your key"}
                  onChange={(event) => setKey(event.target.value)}
                />
              </label>
            )}
            {error && (
              <p className="fr-err" role="alert">
                {error}{" "}
                <button type="button" className="fr-link" onClick={() => list([])}>
                  Type the model name instead
                </button>
              </p>
            )}
          </div>
        )}

        {step === "models" && (
          <div className="fr-body">
            {found.length > 6 && <input className="fr-in" autoFocus value={query} placeholder="Search models" aria-label="Search models" onChange={(event) => setQuery(event.target.value)} />}
            <ul className="fr-models" role="listbox" aria-label="Models">
              {shown.map((model) => (
                <li key={model.id} role="option" aria-selected={model.id === picked}>
                  <button type="button" onClick={() => setPicked(model.id)} onDoubleClick={start}>
                    <b>{model.id}</b>
                    <span>{contextLabel(model.context_length)}</span>
                  </button>
                </li>
              ))}
              {!shown.length && <li className="fr-none">No model matches “{query}”.</li>}
            </ul>
            {unsized && sizeField}
            <button type="button" className="fr-link fr-aside" onClick={() => setStep("manual")}>
              Type a model name instead
            </button>
            {error && (
              <p className="fr-err" role="alert">
                {error}
              </p>
            )}
          </div>
        )}

        {step === "manual" && (
          <div className="fr-body">
            <label>
              Model name
              <input className="fr-in" required autoFocus value={typed} placeholder="As the server names it" onChange={(event) => setTyped(event.target.value)} />
            </label>
            {sizeField}
            {error && (
              <p className="fr-err" role="alert">
                {error}
              </p>
            )}
          </div>
        )}

        <footer className="fr-foot">
          <button type="button" className="fr-link" onClick={onClose}>
            Not now
          </button>
          {step === "connect" && (
            <button className="btn-gold" disabled={busy}>
              {busy ? "Looking…" : error ? "Try again" : "Find models"}
            </button>
          )}
          {(step === "models" || step === "manual") && (
            <button className="btn-gold" disabled={busy || (step === "models" ? !picked : !typed.trim())}>
              {busy ? "Saving…" : "Start"}
            </button>
          )}
        </footer>
      </form>
    </div>,
    document.body,
  );
}
