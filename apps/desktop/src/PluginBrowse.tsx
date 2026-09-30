import { useEffect, useState } from "react";
import { BROWSE_PAGE, BrowseRow, BrowseSearch, Installed, ranked } from "./Browse";
import { Icon } from "./Icon";
import { Spinner } from "./Spinner";
import { useWorkspaceApi } from "./Workspace";

type Listing = { name: string; description: string; category?: string; source: string; installed: boolean };
type Marketplace = { name: string; url: string; commit: string; plugins: Listing[] };
type Check = { level: "ok" | "warn" | "fail"; subject: string; message: string };

type Props = {
  busy: boolean;
  onInstall: (source: string) => Promise<void>;
};

/** Plugin catalogs saved from marketplaces. Browsing reads the saved copies;
 * only adding and refreshing reach the network. */
export function PluginBrowse({ busy, onInstall }: Props) {
  const api = useWorkspaceApi();
  const [markets, setMarkets] = useState<Marketplace[]>();
  const [unfetched, setUnfetched] = useState(0);
  const [source, setSource] = useState("");
  const [working, setWorking] = useState("");
  const [error, setError] = useState("");
  const [problems, setProblems] = useState<string[]>([]);
  const [reload, setReload] = useState(0);
  const [query, setQuery] = useState("");
  const [market, setMarket] = useState("");
  const [all, setAll] = useState(false);

  useEffect(() => {
    let active = true;
    void api
      .extensions("extensions.marketplace.list")
      .then((result) => {
        if (!active) return;
        setMarkets(result.marketplaces as Marketplace[]);
        setUnfetched(Number(result.unfetched ?? 0));
      })
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [reload]);

  async function run(label: string, method: string, params: Record<string, unknown> = {}) {
    setWorking(label);
    setError("");
    try {
      const result = await api.extensions(method, params);
      setProblems((result.problems as string[]) ?? []);
      setReload((value) => value + 1);
      return true;
    } catch (cause) {
      setError(String(cause));
      return false;
    } finally {
      setWorking("");
    }
  }

  const every = (markets ?? []).flatMap((each) => each.plugins.map((listing) => ({ ...listing, market: each.name })));
  const found = ranked(every, query, (listing) => ({ name: listing.name, rest: `${listing.description} ${listing.category ?? ""}` }));
  const matches = market ? found.filter((listing) => listing.market === market) : found;
  const shown = all ? matches : matches.slice(0, BROWSE_PAGE);
  const several = (markets?.length ?? 0) > 1;
  return (
    <div className="browse">
      <BrowseSearch
        label="Search plugins"
        placeholder={markets ? `Search ${every.length} plugins${several ? ` from ${markets.length} marketplaces` : ""}` : "Search plugins"}
        value={query}
        onChange={(value) => {
          setQuery(value);
          setAll(false);
        }}
      />
      {several && (
        <div className="browse-scope" role="group" aria-label="Marketplace">
          {[{ name: "", count: found.length }, ...markets!.map((each) => ({ name: each.name, count: found.filter((listing) => listing.market === each.name).length }))].map((each) => (
            <button
              key={each.name || "all"}
              aria-pressed={market === each.name}
              onClick={() => {
                setMarket(each.name);
                setAll(false);
              }}
            >
              {each.name || "All"} <span>{each.count}</span>
            </button>
          ))}
        </div>
      )}
      {error && <p className="surface-error" role="alert">{error}</p>}
      {problems.map((problem) => (
        <p className="surface-error" key={problem}>{problem}</p>
      ))}
      {markets && !markets.length && !working && (
        <p className="quiet">
          {unfetched ? "Fetch the catalogs Medha ships with, or add a marketplace below." : "No marketplaces yet. Add one below to browse its plugins."}
        </p>
      )}
      {markets && markets.length > 0 && (
        <ul className="browse-list">
          {shown.map((listing) => (
            <BrowseRow
              key={`${listing.market}:${listing.source}`}
              name={listing.name}
              meta={listing.category}
              description={listing.description}
              action={
                listing.installed ? (
                  <Installed />
                ) : (
                  <button
                    className="btn-line"
                    disabled={busy || Boolean(working)}
                    onClick={() => {
                      setWorking(`install:${listing.source}`);
                      void onInstall(listing.source).finally(() => {
                        setWorking("");
                        setReload((value) => value + 1);
                      });
                    }}
                  >
                    {working === `install:${listing.source}` ? "Installing…" : "Install"}
                  </button>
                )
              }
            />
          ))}
          {!matches.length && <li className="quiet">{query ? `No plugins match “${query}”.` : "This marketplace lists no plugins."}</li>}
        </ul>
      )}
      {matches.length > shown.length && (
        <button className="btn-line browse-more" onClick={() => setAll(true)}>
          Show all {matches.length}
        </button>
      )}
      <p className="browse-foot">Installed plugins stay off until you review their access.</p>
      <details className="browse-manage" open={Boolean(markets && !markets.length)}>
        <summary>
          Marketplaces{markets?.length ? ` (${markets.length})` : ""}
        </summary>
        <ul className="manage-list">
          {(markets ?? []).map((each) => (
            <li key={each.name}>
              <b>{each.name}</b>
              <span className="manage-status">
                {each.url} · {each.commit} · {each.plugins.length} {each.plugins.length === 1 ? "plugin" : "plugins"}
              </span>
              <button
                className="btn-line"
                disabled={Boolean(working)}
                title="Its installed plugins stay installed"
                onClick={() => void run(`remove:${each.name}`, "extensions.marketplace.remove", { name: each.name })}
              >
                Remove
              </button>
            </li>
          ))}
        </ul>
        <div className="browse-manage-add">
          <form
            className="plugin-install"
            onSubmit={(event) => {
              event.preventDefault();
              void run("add", "extensions.marketplace.add", { source }).then((ok) => ok && setSource(""));
            }}
          >
            <label>
              <Icon name="plus" />
              <input
                required
                aria-label="Marketplace repository"
                value={source}
                onChange={(event) => setSource(event.target.value)}
                placeholder="Add a marketplace: owner/repo or a git URL"
              />
            </label>
            <button className="btn-line" disabled={Boolean(working) || !source.trim()}>
              {working === "add" ? "Adding…" : "Add"}
            </button>
          </form>
          <button className="btn-line" disabled={Boolean(working)} onClick={() => void run("refresh", "extensions.marketplace.refresh")}>
            {working === "refresh" ? <Spinner /> : <Icon name="refresh" />}
            {unfetched ? "Fetch catalogs" : "Refresh all"}
          </button>
        </div>
      </details>
    </div>
  );
}

/** What would stop an enabled plugin from working, and how to fix it. */
export function PluginDoctor() {
  const api = useWorkspaceApi();
  const [checks, setChecks] = useState<Check[]>();
  const [running, setRunning] = useState(false);
  const [error, setError] = useState("");
  async function check() {
    setRunning(true);
    setError("");
    try {
      setChecks((await api.extensions("extensions.doctor")).checks as Check[]);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setRunning(false);
    }
  }
  const failing = checks?.filter((item) => item.level !== "ok").length ?? 0;
  return (
    <section className="plugin-doctor">
      <div className="doctor-head">
        <button className="btn-line" disabled={running} onClick={() => void check()}>
          {running ? <Spinner /> : <Icon name="shield" />}
          Check plugin health
        </button>
        {checks && <span>{failing ? `${failing} ${failing === 1 ? "problem" : "problems"} found` : "Everything looks good"}</span>}
      </div>
      {error && <p className="surface-error" role="alert">{error}</p>}
      {checks && (
        <ul className="doctor-checks">
          {checks.map((item, index) => (
            <li key={index} className={`doctor-${item.level}`}>
              <Icon name={item.level === "ok" ? "check" : "x"} />
              <b>{item.subject}</b>
              <span>{item.message}</span>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
