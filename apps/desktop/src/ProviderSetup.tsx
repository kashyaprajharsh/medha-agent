import { useEffect, useRef, useState } from "react";
import { useWorkspaceApi } from "./Workspace";
import type { ModelTestResult, Provider } from "./modelSetup";

/** Endpoint values are explicit and validated by the shared backend. */
export function ProviderFields({ provider, disabled, onChange, onResolved }: {
  provider: Provider; disabled: boolean; onChange: () => void; onResolved: (url: string) => void;
}) {
  const api = useWorkspaceApi();
  const [values, setValues] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const generation = useRef(0);
  useEffect(() => () => { generation.current++; }, []);
  async function resolve() {
    const mine = ++generation.current;
    setBusy(true);
    setError("");
    try {
      const reply = await api.settings("application.providers.resolve", { provider_id: provider.id, parameters: values });
      if (mine === generation.current) onResolved(reply.base_url as string);
    } catch (cause) { if (mine === generation.current) setError(String(cause).replace(/^Error:\s*/, "")); }
    finally { if (mine === generation.current) setBusy(false); }
  }
  return <div>
    {(provider.parameters ?? []).map((name) => <label key={name}>
      {name.replaceAll("_", " ")}
      <input value={values[name] ?? ""} disabled={disabled || busy} autoComplete="off" onChange={(event) => { setValues({ ...values, [name]: event.target.value }); onChange(); setError(""); }} />
    </label>)}
    <p className="quiet">These values complete the endpoint. Enter your API key separately.</p>
    <button type="button" className="btn-line" disabled={disabled || busy || !(provider.parameters ?? []).every((name) => values[name]?.trim())} onClick={() => void resolve()}>{busy ? "Checking endpoint…" : "Use endpoint"}</button>
    {error && <p role="alert">{error}</p>}
  </div>;
}

/** A result describes this model/endpoint/key, not every model at the provider. */
export function ModelTest({ result }: { result?: ModelTestResult }) {
  if (!result) return null;
  return <div className="quiet" role="status">
    {(["text", "streaming", "tools"] as const).map((kind) => <p key={kind}><b>{kind === "tools" ? "Functions" : kind === "text" ? "Text" : "Streaming"}: {result[kind].status === "failed" && result[kind].failure === "timeout" ? "timed out" : result[kind].status}</b> — {result[kind].message}</p>)}
  </div>;
}
