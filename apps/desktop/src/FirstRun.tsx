import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { useWorkspaceApi } from "./Workspace";
import { type Protocol, type UnavailableProvider } from "./modelSetup";
import { ConnectList, type Chosen } from "./ConnectList";
import { ChatGptSheet } from "./ChatGpt";
import { ModelForm, ownServers, type Model } from "./Settings";

/**
 * The first thing a new person sees: connect one model, through the same list
 * and form as Settings, centred over the app. What it saves becomes the default.
 */
export function FirstRun({ onDone, onClose }: { onDone: () => void; onClose: () => void }) {
  const api = useWorkspaceApi();
  const [protocols, setProtocols] = useState<Protocol[]>([]);
  const [models, setModels] = useState<Model[]>([]);
  const [unavailable, setUnavailable] = useState<UnavailableProvider[]>([]);
  const [connect, setConnect] = useState<Chosen>();
  const [round, setRound] = useState(0);
  const [chatgpt, setChatgpt] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const [refreshing, setRefreshing] = useState(false);
  const [refreshError, setRefreshError] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    let current = true;
    void api
      .settings()
      .then((result) => {
        if (!current) return;
        setProtocols(result.protocols as Protocol[]);
        setModels((result.models as Model[]) ?? []);
        setUnavailable((result.unavailable_providers as UnavailableProvider[]) ?? []);
      })
      .catch((cause) => current && setError(String(cause)));
    return () => {
      current = false;
    };
  }, [api, refresh]);

  // Esc steps back to the list first; the ChatGPT sheet handles its own.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || chatgpt) return;
      if (connect) setConnect(undefined);
      else onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [chatgpt, connect, onClose]);

  const choose = (chosen: Chosen) => {
    setError("");
    if (chosen.kind === "chatgpt") return setChatgpt(true);
    setConnect(chosen);
    setRound((value) => value + 1);
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

  async function save(params: Record<string, unknown>) {
    setBusy(true);
    setError("");
    try {
      await api.settings("settings.model.save", { ...params, default: true });
      window.dispatchEvent(new CustomEvent("medha-preferences"));
      onDone();
    } catch (cause) {
      setError(String(cause).replace(/^Error:\s*/, ""));
      throw cause;
    } finally {
      setBusy(false);
    }
  }

  if (chatgpt)
    return (
      <ChatGptSheet
        firstRun
        onClose={() => setChatgpt(false)}
        onUseKey={() => setChatgpt(false)}
        onSaved={(name) => {
          window.dispatchEvent(new CustomEvent("medha-preferences", { detail: { profile: name } }));
          onDone();
        }}
      />
    );

  return createPortal(
    <div className="fr-scrim">
      <div className="cx-sheet cx-centre" role="dialog" aria-modal="true" aria-label="Connect a model">
        {error && <p className="fr-err" role="alert">{error}</p>}
        {!connect ? (
          <div className="cx-pick">
            <header className="fr-head">
              <h2>Connect a model</h2>
              <p>Medha needs one model to start. Pick where yours runs.</p>
            </header>
            <ConnectList
              protocols={protocols}
              servers={ownServers(models, protocols)}
              savedKeys={models.filter((item) => item.key_present).map((item) => item.profile.base_url)}
              unavailable={unavailable}
              refreshing={refreshing}
              refreshError={refreshError}
              closeLabel="Not now"
              onClose={onClose}
              onRefresh={() => void refreshCatalogue()}
              onChoose={choose}
            />
          </div>
        ) : (
          <ModelForm
            key={`first-${round}`}
            model={null}
            start={connect}
            saved={models}
            protocols={protocols}
            busy={busy}
            firstRun
            onCancel={onClose}
            onBack={() => setConnect(undefined)}
            onSwitch={(provider, key) => choose({ kind: "provider", provider, key })}
            onSave={save}
          />
        )}
      </div>
    </div>,
    document.body,
  );
}
