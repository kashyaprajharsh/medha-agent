import { useEffect, useRef, useState } from "react";
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
  inputs?: McpPrefill["inputs"];
  unsupported?: string;
};
type Server = {
  name: string;
  title?: string;
  description?: string;
  version?: string;
  repository?: string;
  stars?: number;
  organization?: string;
  setups: Setup[];
};
type Source = "featured" | "full";

function stars(count: number) {
  return count < 1000 ? String(count) : `${(count / 1000).toFixed(count < 10_000 ? 1 : 0)}k`;
}

/** Short, lowercase server name from a registry id like `io.github.org/tool-mcp`. */
function shortName(name: string) {
  const last = name.split("/").at(-1) ?? name;
  return last.replace(/[-_]?mcp[-_]?(server)?$/i, "").replace(/[^a-z0-9-]/gi, "-").toLowerCase() || "server";
}

/** GitHub's curated MCP list first, the full public registry on request. Entries
 * are not reviewed by Medha; choosing one only fills in the add form. */
export function McpCatalog({ onPick }: { onPick: (prefill: McpPrefill) => void }) {
  const api = useWorkspaceApi();
  const [query, setQuery] = useState("");
  const [submitted, setSubmitted] = useState("");
  const [source, setSource] = useState<Source>("featured");
  const [servers, setServers] = useState<Server[]>([]);
  const [next, setNext] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const latest = useRef(0);

  async function load(search: string, cursor: string | null) {
    const request = ++latest.current;
    setLoading(true);
    setError("");
    if (!cursor) {
      setServers([]);
      setNext(null);
    }
    try {
      const result = await api.extensions("extensions.mcp.registry", { query: search, cursor, source });
      if (request !== latest.current) return;
      const rows = result.servers as Server[];
      setServers((previous) => {
        const combined = cursor ? [...previous, ...rows] : rows;
        // Star rankings can move between page requests.
        return combined.filter((server, index) => combined.findIndex((other) => other.name === server.name && other.version === server.version) === index);
      });
      setNext((result.next as string | null) ?? null);
    } catch (cause) {
      if (request === latest.current) setError(String(cause).replace(/^Error:\s*/, ""));
    } finally {
      if (request === latest.current) setLoading(false);
    }
  }

  useEffect(() => {
    void load(submitted, null);
  }, [submitted, source]);

  function pick(server: Server, setup: Setup) {
    onPick({
      id: shortName(server.name),
      transport: setup.kind,
      url: setup.url,
      signIn: setup.sign_in === "token" ? "token" : "none",
      tokenHelp: setup.token_help,
      command: setup.command,
      variables: setup.variables,
      inputs: setup.inputs,
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
          <input aria-label="Search MCP servers" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search servers, e.g. github, postgres, notion" />
        </label>
        <button className="btn-line" disabled={loading}>Search</button>
      </form>
      <p className="quiet">
        {source === "featured"
          ? "Popular servers from GitHub’s MCP list, most-starred first."
          : "Every server in the public MCP Registry. "}
        {source === "full" && (
          <button className="link-btn" onClick={() => setSource("featured")}>Back to GitHub’s list</button>
        )}{" "}
        Listings are published by their authors and aren’t reviewed by Medha — check the source before you connect.
      </p>
      {loading && !servers.length && (
        <p className="quiet" role="status">
          <Spinner />{" "}
          {source === "full"
            ? submitted
              ? `Searching the full registry for “${submitted}”… it can take a little while.`
              : "Loading the full registry…"
            : "Searching…"}
        </p>
      )}
      {error && <p className="surface-error" role="alert">{error}</p>}
      <ul className="catalog-list">
        {servers.map((server) => {
          const usable = server.setups.filter((setup) => !setup.unsupported);
          return (
            <li key={`${server.name}@${server.version}`}>
              <div className="catalog-main">
                <b>
                  {server.title || server.name}
                  {server.organization && (
                    <span className="default-label" title={`Published by the ${server.organization} organization on GitHub`}>
                      Official
                    </span>
                  )}
                </b>
                {server.description && <p>{server.description}</p>}
                <small>
                  {server.name}
                  {server.version && ` · ${server.version}`}
                  {typeof server.stars === "number" && ` · ★ ${stars(server.stars)}`}
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
      {loading && servers.length > 0 && <p className="quiet"><Spinner /> Loading more…</p>}
      {!loading && !servers.length && !error && (
        <div className="catalog-empty">
          <p className="quiet">
            {submitted
              ? `Nothing in ${source === "featured" ? "GitHub’s list" : "the registry"} matches “${submitted}”.`
              : "No servers found."}{" "}
            {source === "featured" ? "The full registry has many more." : "If you know the server’s address, add it yourself."}
          </p>
          <div className="row-actions">
            {source === "featured" && (
              <button className="btn-line" onClick={() => setSource("full")}>
                Search the full registry
              </button>
            )}
            <button className="btn-line" onClick={() => onPick({ id: shortName(submitted || "server"), transport: "remote" })}>
              Add by address
            </button>
          </div>
        </div>
      )}
      {next && !loading && (
        <button className="more" onClick={() => void load(submitted, next)}>Show more</button>
      )}
    </div>
  );
}
