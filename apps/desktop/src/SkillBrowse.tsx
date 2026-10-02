import { useEffect, useState } from "react";
import { BROWSE_PAGE, BrowseRow, BrowseSearch, Installed, ranked } from "./Browse";
import { Icon } from "./Icon";
import { Spinner } from "./Spinner";
import type { WorkspaceApi } from "./api";
import { useWorkspaceApi } from "./Workspace";

type Hit = { name: string; description: string; repo: string; install_url: string };
type Catalog = { hits: Hit[]; errors: string[]; sources: number };

// Listing a source costs GitHub API calls, which are rate-limited without a
// token: read each catalog once per app run and filter locally.
let catalog: Promise<Catalog> | undefined;
export function forgetSkillCatalog() {
  catalog = undefined;
}
function load(api: WorkspaceApi) {
  catalog ??= api.extensions("extensions.skills.search", { query: "" }).then((result) => result as unknown as Catalog);
  catalog.catch(forgetSkillCatalog);
  return catalog;
}

/** Skills published in the sources Medha reads. Installing one goes through the same scan as a pasted link. */
export function SkillBrowse({ installed, busy, onInstall }: { installed: Set<string>; busy: boolean; onInstall: (source: string) => Promise<void> }) {
  const api = useWorkspaceApi();
  const [found, setFound] = useState<Catalog>();
  const [error, setError] = useState("");
  const [query, setQuery] = useState("");
  const [all, setAll] = useState(false);
  const [installing, setInstalling] = useState("");
  const [reload, setReload] = useState(0);

  useEffect(() => {
    let active = true;
    setError("");
    load(api)
      .then((result) => active && setFound(result))
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [reload]);

  const hits = ranked(found?.hits ?? [], query, (hit) => ({ name: hit.name, rest: hit.description }));
  const shown = all ? hits : hits.slice(0, BROWSE_PAGE);
  const sources = found?.sources ?? 0;
  return (
    <div className="browse">
      <BrowseSearch
        label="Search skills"
        placeholder={found ? `Search ${found.hits.length} skills from ${sources} ${sources === 1 ? "source" : "sources"}` : "Search skills"}
        value={query}
        onChange={(value) => {
          setQuery(value);
          setAll(false);
        }}
      />
      {error && (
        <p className="surface-error" role="alert">
          Couldn't read skill sources: {error}{" "}
          <button className="link-btn" onClick={() => setReload((value) => value + 1)}>
            Try again
          </button>
        </p>
      )}
      {found?.errors.map((problem) => (
        <p className="surface-error" key={problem}>
          {problem}
        </p>
      ))}
      {!found && !error && (
        <p className="quiet browse-loading">
          <Spinner /> Reading skill sources…
        </p>
      )}
      {found && (
        <ul className="browse-list">
          {shown.map((hit) => (
            <BrowseRow
              key={hit.install_url}
              name={hit.name}
              meta={hit.repo}
              several={sources > 1}
              description={hit.description}
              action={
                installed.has(hit.name) ? (
                  <Installed />
                ) : (
                  <button
                    className="btn-line"
                    disabled={busy || Boolean(installing)}
                    onClick={() => {
                      setInstalling(hit.install_url);
                      void onInstall(hit.install_url).finally(() => setInstalling(""));
                    }}
                  >
                    {installing === hit.install_url ? "Installing…" : "Install"}
                  </button>
                )
              }
            />
          ))}
          {!hits.length && <li className="quiet">{query ? `No skills match “${query}”.` : "These sources list no skills."}</li>}
        </ul>
      )}
      {hits.length > shown.length && (
        <button className="btn-line browse-more" onClick={() => setAll(true)}>
          Show all {hits.length}
        </button>
      )}
      {found && (
        <p className="browse-foot">
          <button
            className="link-btn"
            onClick={() => {
              forgetSkillCatalog();
              setFound(undefined);
              setReload((value) => value + 1);
            }}
          >
            <Icon name="refresh" /> Refresh
          </button>
          Add or remove sources under Manage.
        </p>
      )}
    </div>
  );
}
