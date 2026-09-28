import { useEffect, useState } from "react";
import { Icon } from "./Icon";
import type { McpPrefill } from "./McpServers";
import { Spinner } from "./Spinner";
import { useWorkspaceApi } from "./Workspace";

type Setup = {
  kind: "remote" | "local";
  url?: string;
  package?: string;
  sign_in?: "detect" | "token";
  token_help?: string;
  command?: string[];
  variables?: McpPrefill["variables"];
  unsupported?: string;
};
type Server = { name: string; title?: string; description?: string; version?: string; repository?: string; setups: Setup[] };

/** Short, lowercase server name from a registry id like `io.github.org/tool-mcp`. */
function shortName(name: string) {
  const last = name.split("/").at(-1) ?? name;
  return last.replace(/[-_]?mcp[-_]?(server)?$/i, "").replace(/[^a-z0-9-]/gi, "-").toLowerCase() || "server";
}

/** The public MCP Registry, searched on demand. Entries are not reviewed by
 * Medha; choosing one only fills in the add form. */
export function McpCatalog({ onPick }: { onPick: (prefill: McpPrefill) => void }) {
  const api = useWorkspaceApi();
  const [query, setQuery] = useState("");
  const [submitted, setSubmitted] = useState("");
  const [servers, setServers] = useState<Server[]>([]);
  const [next, setNext] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");

  async function load(search: string, cursor: string | null) {
    setLoading(true);
    setError("");
    try {
      const result = await api.extensions("extensions.mcp.registry", { query: search, cursor });
      const rows = result.servers as Server[];
      setServers((previous) => (cursor ? [...previous, ...rows] : rows));
      setNext((result.next as string | null) ?? null);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void load(submitted, null);
  }, [submitted]);

  function pick(server: Server, setup: Setup) {
    onPick({
      id: shortName(server.name),
      transport: setup.kind,
      url: setup.url,
      signIn: setup.sign_in === "token" ? "token" : "none",
      tokenHelp: setup.token_help,
      command: setup.command,
      variables: setup.variables,
    });
  }

  return (
    <div className="mcp-catalog">
      <form
        className="plugin-install"
        onSubmit={(event) => {
          event.preventDefault();
          setSubmitted(query);
        }}
      >
        <label>
          <Icon name="search" />
          <input aria-label="Search the MCP Registry" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search servers, e.g. github, postgres, notion" />
        </label>
        <button className="btn-line" disabled={loading}>Search</button>
      </form>
      <p className="quiet">From the public MCP Registry. Listings are published by their authors and aren’t reviewed by Medha — check the source before you connect.</p>
      {error && <p className="surface-error" role="alert">{error}</p>}
      <ul className="catalog-list">
        {servers.map((server) => {
          const usable = server.setups.filter((setup) => !setup.unsupported);
          return (
            <li key={`${server.name}@${server.version}`}>
              <div className="catalog-main">
                <b>{server.title || server.name}</b>
                {server.description && <p>{server.description}</p>}
                <small>
                  {server.name}
                  {server.version && ` · ${server.version}`}
                  {server.repository && (
                    <>
                      {" · "}
                      <button className="link-btn" onClick={() => void api.openLink(server.repository!)}>Source</button>
                    </>
                  )}
                </small>
                {!usable.length && server.setups[0]?.unsupported && (
                  <small className="catalog-why">Can’t set up yet: it {server.setups[0].unsupported}.</small>
                )}
              </div>
              <div className="catalog-actions">
                {usable.map((setup, index) => (
                  <button key={index} className="btn-line" onClick={() => pick(server, setup)}>
                    {usable.length > 1 ? (setup.kind === "remote" ? "Set up · remote" : "Set up · local") : "Set up"}
                  </button>
                ))}
              </div>
            </li>
          );
        })}
      </ul>
      {loading && <p className="quiet"><Spinner /> Searching…</p>}
      {!loading && !servers.length && !error && <p className="quiet">No servers found.</p>}
      {next && !loading && (
        <button className="more" onClick={() => void load(submitted, next)}>Show more</button>
      )}
    </div>
  );
}
