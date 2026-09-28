import { useEffect, useState } from "react";
import { Icon } from "./Icon";
import { Spinner } from "./Spinner";
import { useWorkspaceApi } from "./Workspace";

type Listing = { name: string; description: string; category?: string; source: string; installed: boolean };
type Marketplace = { name: string; url: string; commit: string; plugins: Listing[] };
type Check = { level: "ok" | "warn" | "fail"; subject: string; message: string };

type Props = {
  query: string;
  busy: boolean;
  onInstall: (source: string) => Promise<void>;
};

/** Plugin catalogs saved from marketplaces. Browsing reads the saved copies;
 * only adding and refreshing reach the network. */
export function PluginBrowse({ query, busy, onInstall }: Props) {
  const api = useWorkspaceApi();
  const [markets, setMarkets] = useState<Marketplace[]>([]);
  const [unfetched, setUnfetched] = useState(0);
  const [source, setSource] = useState("");
  const [working, setWorking] = useState("");
  const [error, setError] = useState("");
  const [problems, setProblems] = useState<string[]>([]);
  const [reload, setReload] = useState(0);

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

  const needle = query.toLowerCase();
  const shown = (listing: Listing) => `${listing.name} ${listing.description} ${listing.category ?? ""}`.toLowerCase().includes(needle);
  return (
    <div className="plugin-browse">
      <div className="page-heading">
        <div>
          <h2>Browse plugins</h2>
          <p>From the marketplaces you’ve added. Installed plugins stay off until you review their access.</p>
        </div>
        <button className="btn-line" disabled={Boolean(working)} onClick={() => void run("refresh", "extensions.marketplace.refresh")}>
          {working === "refresh" ? <Spinner /> : <Icon name="refresh" />}
          {unfetched ? "Fetch catalogs" : "Refresh"}
        </button>
      </div>
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
      {error && <p className="surface-error" role="alert">{error}</p>}
      {problems.map((problem) => (
        <p className="surface-error" key={problem}>{problem}</p>
      ))}
      {!markets.length && !working && (
        <p className="quiet">
          {unfetched ? "Fetch the catalogs Medha ships with, or add a marketplace." : "No marketplaces yet. Add one to browse its plugins."}
        </p>
      )}
      {markets.map((market) => {
        const listings = market.plugins.filter(shown);
        return (
          <section className="market" key={market.name}>
            <header className="market-head">
              <div>
                <b>{market.name}</b>
                <small>
                  {market.url} · {market.commit} · {market.plugins.length} {market.plugins.length === 1 ? "plugin" : "plugins"}
                </small>
              </div>
              <button
                className="btn-line"
                disabled={Boolean(working)}
                title="Its installed plugins stay installed"
                onClick={() => void run(`remove:${market.name}`, "extensions.marketplace.remove", { name: market.name })}
              >
                Remove
              </button>
            </header>
            <ul className="market-plugins">
              {listings.map((listing) => (
                <li key={listing.source}>
                  <div>
                    <b>{listing.name}</b>
                    {listing.category && <small>{listing.category}</small>}
                    {listing.description && <p>{listing.description}</p>}
                  </div>
                  {listing.installed ? (
                    <span className="market-installed">
                      <Icon name="check" />
                      Installed
                    </span>
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
                  )}
                </li>
              ))}
              {!listings.length && <li className="quiet">{query ? "No plugins match." : "This marketplace lists no plugins."}</li>}
            </ul>
          </section>
        );
      })}
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
