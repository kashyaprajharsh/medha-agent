import { useEffect, useState } from "react";
import { Icon } from "./Icon";
import { Select } from "./Select";
import { useWorkspaceApi } from "./Workspace";

export type McpServer = {
  id: string;
  hash: string;
  command: string[];
  url: string;
  executable?: string;
  disabled: boolean;
  auth: string;
  trust: string;
  key_present: boolean;
  signed_in: boolean;
  env_names: string[];
  allow_tools: string[];
  deny_tools: string[];
  network: boolean | null;
  parallel: boolean;
};

export type McpStatus = { state: string; tools: number; hidden?: number; detail?: string };

type Variable = { name: string; value: string; hint?: string; secret?: boolean };

/** A catalog entry's chosen setup, used to fill the form in. */
export type McpPrefill = {
  id: string;
  transport: "remote" | "local";
  url?: string;
  signIn?: "none" | "token";
  tokenHelp?: string;
  command?: string[];
  variables?: { name: string; description?: string; required: boolean; secret: boolean; default?: string | null }[];
};

/** Everything `/mcp add` takes, as a form: remote servers sign in with OAuth or a
 * token, local ones get an API key and environment; secrets go to the keychain. */
export function McpForm({ busy, onSave, initial }: { busy: boolean; onSave: (args: string[]) => Promise<void>; initial?: McpPrefill }) {
  const secrets = initial?.variables?.filter((variable) => variable.secret) ?? [];
  const [id, setId] = useState(initial?.id ?? "");
  const [transport, setTransport] = useState<string>(initial?.transport ?? "remote");
  const [url, setUrl] = useState(initial?.url ?? "");
  const [signIn, setSignIn] = useState<string>(initial?.signIn ?? "none");
  const [token, setToken] = useState("");
  const [command, setCommand] = useState(initial?.command?.[0] ?? "npx");
  const [argumentsText, setArguments] = useState(initial?.command ? initial.command.slice(1).join("\n") : "-y\nserver-package");
  const [key, setKey] = useState("");
  const [keyVariable, setKeyVariable] = useState(secrets[0]?.name ?? "API_KEY");
  const [variables, setVariables] = useState<Variable[]>(
    (initial?.variables ?? [])
      .filter((variable) => variable !== secrets[0])
      .map((variable) => ({ name: variable.name, value: variable.default ?? "", hint: variable.description, secret: variable.secret })),
  );
  const [trusted, setTrusted] = useState(false);
  const [network, setNetwork] = useState(true);
  const [parallel, setParallel] = useState(false);
  const [error, setError] = useState("");

  function args() {
    const flags = [id.trim()];
    if (!flags[0]) throw new Error("Enter a server name");
    if (trusted) flags.push("--trust", "trusted");
    if (!network) flags.push("--no-network");
    if (parallel) flags.push("--parallel");
    if (transport === "remote") {
      if (signIn === "oauth") flags.push("--oauth");
      if (signIn === "token") {
        if (!token) throw new Error("Paste the token, or choose another sign-in");
        flags.push("--bearer", token);
      }
      return [...flags, "--url", url.trim()];
    }
    if (key) {
      if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(keyVariable)) throw new Error("The key's variable name must be letters, digits or _");
      flags.push("--key", key, "--env", `${keyVariable}=\${key}`);
    }
    for (const variable of variables) {
      if (!variable.name.trim() || !variable.value) continue;
      if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(variable.name)) throw new Error(`${variable.name} is not a valid variable name`);
      flags.push("--env", `${variable.name}=${variable.value}`);
    }
    if (!command.trim()) throw new Error("Enter an executable");
    return [...flags, "--", command.trim(), ...argumentsText.split("\n").filter((line) => line.length > 0)];
  }

  return (
    <form
      className="settings-form mcp-form"
      onSubmit={(event) => {
        event.preventDefault();
        setError("");
        try {
          void onSave(args()).then(() => {
            setKey("");
            setToken("");
          });
        } catch (cause) {
          setError(cause instanceof Error ? cause.message : String(cause));
        }
      }}
    >
      <label>
        Server name
        <input required value={id} onChange={(event) => setId(event.target.value)} placeholder="github" />
      </label>
      <label>
        Connection
        <Select value={transport} onChange={(event) => setTransport(event.target.value)}>
          <option value="remote">Remote URL</option>
          <option value="local">Local process</option>
        </Select>
      </label>
      {transport === "remote" ? (
        <>
          <label>
            Server URL
            <input required type="url" value={url} onChange={(event) => setUrl(event.target.value)} placeholder="https://" />
          </label>
          <label>
            Sign-in
            <Select value={signIn} onChange={(event) => setSignIn(event.target.value)}>
              <option value="none">Detect when connecting</option>
              <option value="oauth">Sign in with the browser (OAuth)</option>
              <option value="token">API key or token</option>
            </Select>
          </label>
          {signIn === "token" && (
            <label>
              API key or token
              <input type="password" autoComplete="new-password" value={token} onChange={(event) => setToken(event.target.value)} />
            </label>
          )}
          <p className="quiet">
            {signIn === "oauth"
              ? "Connecting opens your browser to sign in. Medha keeps the sign-in in your keychain."
              : signIn === "token"
                ? `${initial?.tokenHelp ? `${initial.tokenHelp}. ` : ""}The token is kept in your keychain, never in a file.`
                : "If the server asks for sign-in, Medha opens your browser when you connect."}
          </p>
        </>
      ) : (
        <>
          <label>
            Executable
            <input required value={command} onChange={(event) => setCommand(event.target.value)} placeholder="npx or /path/to/server" />
          </label>
          <label>
            Arguments
            <textarea value={argumentsText} onChange={(event) => setArguments(event.target.value)} placeholder="One argument per line" />
          </label>
          <div className="mcp-key">
            <label>
              API key (optional)
              <input type="password" autoComplete="new-password" value={key} onChange={(event) => setKey(event.target.value)} />
            </label>
            <label>
              Passed to the server as
              <input value={keyVariable} onChange={(event) => setKeyVariable(event.target.value)} spellCheck={false} />
            </label>
          </div>
          <fieldset className="mcp-env">
            <legend>Environment variables</legend>
            {variables.map((variable, index) => (
              <div className="mcp-env-row" key={index}>
                <input aria-label="Variable name" placeholder="NAME" value={variable.name} spellCheck={false}
                  onChange={(event) => setVariables(variables.map((row, at) => (at === index ? { ...row, name: event.target.value } : row)))} />
                <input aria-label="Variable value" placeholder={variable.hint || "value"} title={variable.hint} value={variable.value}
                  type={variable.secret ? "password" : "text"}
                  onChange={(event) => setVariables(variables.map((row, at) => (at === index ? { ...row, value: event.target.value } : row)))} />
                <button type="button" className="icon-btn" aria-label="Remove variable" onClick={() => setVariables(variables.filter((_, at) => at !== index))}>
                  <Icon name="x" />
                </button>
              </div>
            ))}
            <button type="button" className="btn-line" onClick={() => setVariables([...variables, { name: "", value: "" }])}>
              <Icon name="plus" />
              Add variable
            </button>
            <p className="quiet">Put secrets in API key, not here: variables are saved in your config file.</p>
            {variables.some((variable) => variable.secret) && (
              <p className="surface-error">
                This server needs more than one secret. Only the API key goes to your keychain; {variables.filter((variable) => variable.secret).map((variable) => variable.name).join(", ")} would be saved in your config file.
              </p>
            )}
          </fieldset>
        </>
      )}
      <details className="mcp-advanced">
        <summary>Access</summary>
        <label className="checkbox-label">
          <input type="checkbox" checked={trusted} onChange={(event) => setTrusted(event.target.checked)} />
          Trusted: connect on start without asking
        </label>
        <label className="checkbox-label">
          <input type="checkbox" checked={network} onChange={(event) => setNetwork(event.target.checked)} />
          Allow network access
        </label>
        <label className="checkbox-label">
          <input type="checkbox" checked={parallel} onChange={(event) => setParallel(event.target.checked)} />
          Allow parallel tool calls
        </label>
      </details>
      {error && (
        <p role="alert" className="surface-error">
          {error}
        </p>
      )}
      <button className="btn-gold" disabled={busy}>
        Save server
      </button>
    </form>
  );
}

type DetailsProps = {
  server: McpServer;
  status?: McpStatus;
  tools: { name: string; description: string }[];
  busy: boolean;
  onUpdate: (patch: Record<string, unknown>) => void;
  onSignIn?: () => void;
  onSignOut: () => void;
};

/** A saved server's sign-in, secrets (named, never shown), tools and access. */
export function McpDetails({ server, status, tools, busy, onUpdate, onSignIn, onSignOut }: DetailsProps) {
  const prefix = `mcp__${server.id}__`;
  const exposed = tools.filter((tool) => tool.name.startsWith(prefix)).map((tool) => ({ name: tool.name.slice(prefix.length), description: tool.description }));
  const rows = [...exposed, ...server.deny_tools.filter((name) => !exposed.some((tool) => tool.name === name)).map((name) => ({ name, description: "" }))];
  const connected = status?.state === "ready";
  const toggle = (name: string, on: boolean) =>
    onUpdate({ deny_tools: on ? server.deny_tools.filter((denied) => denied !== name) : [...server.deny_tools, name] });
  return (
    <div className="mcp-details">
      <section>
        <h4>Sign-in</h4>
        {server.auth === "oauth" || server.signed_in ? (
          <div className="mcp-line">
            <span>{server.signed_in ? "Signed in with OAuth" : "Not signed in"}</span>
            {onSignIn && (
              <button className="btn-line" disabled={busy} onClick={onSignIn}>
                {server.signed_in ? "Sign in again" : "Sign in"}
              </button>
            )}
            {server.signed_in && (
              <button className="btn-line" disabled={busy} onClick={onSignOut}>
                Sign out
              </button>
            )}
          </div>
        ) : (
          <p className="quiet">
            {server.key_present
              ? server.url
                ? "Token saved in your keychain"
                : "API key saved in your keychain"
              : server.url
                ? "No sign-in. If the server asks, connecting opens your browser."
                : "No API key"}
          </p>
        )}
        {server.env_names.length > 0 && <p className="quiet">Environment: {server.env_names.join(", ")}</p>}
      </section>
      <section>
        <h4>
          Tools
          {connected && <small>{status.tools} on{status.hidden ? ` · ${status.hidden} off` : ""}</small>}
        </h4>
        {server.allow_tools.length > 0 && <p className="quiet">Only these may be used: {server.allow_tools.join(", ")}</p>}
        {rows.length ? (
          <ul className="mcp-tools">
            {rows.map((tool) => (
              <li key={tool.name}>
                <label className="checkbox-label" title={tool.description}>
                  <input type="checkbox" disabled={busy} checked={!server.deny_tools.includes(tool.name)} onChange={(event) => toggle(tool.name, event.target.checked)} />
                  <code>{tool.name}</code>
                  {tool.description && <span>{tool.description}</span>}
                </label>
              </li>
            ))}
          </ul>
        ) : (
          <p className="quiet">{connected ? "This server offers no tools." : "Connect to see this server’s tools."}</p>
        )}
        {server.deny_tools.length > 0 && <p className="quiet">Turned-off tools stay off after you reconnect.</p>}
      </section>
      <section>
        <h4>Access</h4>
        <label className="checkbox-label">
          <input type="checkbox" disabled={busy} checked={server.trust === "trusted"} onChange={(event) => onUpdate({ trust: event.target.checked ? "trusted" : "workspace" })} />
          Trusted: connect on start without asking
        </label>
        <label className="checkbox-label">
          <input type="checkbox" disabled={busy} checked={server.network !== false} onChange={(event) => onUpdate({ network: event.target.checked ? null : false })} />
          Allow network access
        </label>
        <label className="checkbox-label">
          <input type="checkbox" disabled={busy} checked={server.parallel} onChange={(event) => onUpdate({ parallel: event.target.checked })} />
          Allow parallel tool calls
        </label>
      </section>
    </div>
  );
}

export type SignInEvent = { server: string; url?: string; ok?: boolean; error?: string };

/** Sign-in progress for one chat's bridge: the browser link, then the outcome. */
export function useMcpSignIn(sessionKey: string | null, onDone: (event: SignInEvent) => void) {
  const api = useWorkspaceApi();
  const [pending, setPending] = useState<SignInEvent>();
  useEffect(() => {
    if (!sessionKey) return;
    let off: (() => void) | undefined;
    let gone = false;
    void api
      .onLive((key, frame) => {
        if (key !== sessionKey) return;
        const params = (frame.params ?? {}) as SignInEvent;
        if (frame.method === "mcp.auth") setPending(params);
        if (frame.method === "mcp.signed_in") {
          setPending(undefined);
          onDone(params);
        }
      })
      .then((unlisten) => (gone ? unlisten() : (off = unlisten)));
    return () => {
      gone = true;
      off?.();
    };
  }, [sessionKey]);
  return { pending, start: (server: string) => setPending({ server }) };
}
